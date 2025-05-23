mod fields;

pub use fields::*;
use gen_analyzer::{Binds, PropComponent};
use proc_macro2::TokenStream;
use rssyin::bridger::PropItem;
use std::collections::{HashMap, HashSet};

use crate::{
    builtin::BuiltinWidget,
    model::{
        traits::{CRef, CallbackStmt, HandleEvent, ImplLiveHook, LiveHookType},
        TemplatePtrs,
    },
    script::{Impls, LiveComponent},
    str_to_tk,
    two_way_binding::{GetSet, TWBPollBuilder},
};
use gen_utils::error::{CompilerError, Error};
use quote::{quote, ToTokens};
use syn::{parse_quote, Attribute, Fields, ItemStruct, Stmt};

use super::SugarScript;

/// # Visitor for the widget prop
/// ## 功能1: 双向绑定
/// 这个visitor主要用于处理widget的prop，当开发者定义prop中中的字段后，我们需要为这些字段生成get和set方法, 这非常重要，因为这是响应式双向绑定的基础
/// See [TWBPollBuilder](crate::two_way_binding::TWBPollBuilder) for more information
/// ## 功能2: 组件实例初始化
/// 使用者若用用了Default trait对prop struct进行了初始化，那么我们需要将Default trait中的代码转为组件修饰的代码
/// ```
/// #[component]
/// pub struct AProp{
///     name: String
/// }
///
/// impl Default for AProp{
///     fn default() -> Self{AProp{name: "John".to_string()}}
/// }
/// // --- 转为 ------------------------------------------------
/// #[derive(Live)]
/// pub struct AProp{
///     #[deref]
///     pub deref_widget: GView,
///     #[live]
///     name: String
/// }
///
/// impl Default for APropDeref{
///     fn default() -> Self{Self{name: "John".to_string()}}
/// }
///
/// impl LiveHook for AProp{
///     fn after_new_from_doc(&mut self, _cx:&mut Cx) {
///         self.deref_prop = AProp::default();
///     }
/// }
/// // --------------------------------------------｜
///                                                ｜
/// pub struct APropDeref{                  属性结构体会被生成这个解构体
///    pub name: String                            ｜
/// }                                              ｜
/// // --------------------------------------------｜
/// ```
/// ## 功能3: SugarScript
/// SugarScript也要在PropLzVisitor中进行处理
pub struct PropLzVisitor;

impl PropLzVisitor {
    /// ## 处理组件实例初始化的代码
    /// - 最终将生成一个LiveComponent(组件结构体)
    /// - 将传入的prop改造为属性解构体
    /// - 在impls中添加LiveHook(after new from doc)的实现
    fn instance(
        prop: &mut ItemStruct,
        impls: &mut Impls,
        binds: Option<&Binds>,
    ) -> Result<LiveComponent, Error> {
        let ident = prop.ident.to_token_stream();
        let mut live_component = LiveComponent::default(&ident);
        // [处理解构体] -----------------------------------------------------------------------------------------
        // - [为ident添加Deref作为新结构体名] ---------------------------------------------------------------------
        let ident_tk = str_to_tk!(&format!("{}Deref", ident.to_string()))?;
        prop.ident = parse_quote!(#ident_tk);
        // - [去除prop宏] ---------------------------------------------------------------------------------------
        let attrs = prop
            .attrs
            .iter()
            .filter(|attr| !attr.path().is_ident("component"))
            .map(|attr| attr.clone())
            .collect::<Vec<Attribute>>();

        prop.attrs = attrs;

        // [构建一个LiveComponent] -------------------------------------------------------------------------------
        if !prop.fields.is_empty() {
            // [添加Default] -------------------------------------------------------------------------------------
            let deref_ident = prop.ident.to_token_stream();
            impls.traits().live_hook.push(
                quote! {
                    let deref_prop = #deref_ident::default();
                },
                LiveHookType::AfterNewFromDoc,
            );

            for field in prop.fields.clone().iter_mut() {
                // - [遍历fields并添加live或rust宏] ----------------------------------------------------------------------
                handle_field_attrs(field)?;
                live_component.push_field(field.clone())?;
                // [在impls中添加LiveHook(after new from doc)的实现] -----------------------------------------------------
                let mut tk = TokenStream::new();
                if let Some(binds) = binds {
                    let field_name = field.ident.as_ref().unwrap().to_string();
                    // 需要判断当前这个field是否在模版里进行了绑定，只有绑定的才能生产set方法，否则是正常赋值
                    if binds.contains_key(&field_name) {
                        let field_name = str_to_tk!(&field_name)?;
                        let set_field_fn = str_to_tk!(&format!("set_{}", field_name))?;
                        tk.extend(quote! {
                            self.#set_field_fn(cx, deref_prop.#field_name);
                        });
                    }
                } else {
                    let field_name = field.ident.as_ref().unwrap().to_string();
                    let field_name = str_to_tk!(&field_name)?;
                    tk.extend(quote! {
                        self.#field_name = deref_prop.#field_name;
                    });
                };
                impls
                    .traits()
                    .live_hook
                    .push(tk, LiveHookType::AfterNewFromDoc);
            }
        }

        Ok(live_component)
    }

    /// ## 处理双向绑定
    /// - 生成get和set方法
    /// - 生成双向绑定的代码
    fn two_way_binding(
        component_ident: TokenStream,
        live_component: &mut LiveComponent,
        deref_prop: &ItemStruct,
        binds: &Binds,
        template_ptrs: &TemplatePtrs,
        impls: &mut Impls,
    ) -> Result<Option<TWBPollBuilder>, Error> {
        // [生成get和set方法] -----------------------------------------------------------------------------------
        let mut twb_poll = HashMap::new();

        for field in deref_prop.fields.iter() {
            // - [根据binds生成相关双向绑定的getter setter] -------------------------------------------------------
            let field_ident = field.ident.as_ref().unwrap().to_string();
            let field_ty = field.ty.to_token_stream().to_string();
            let _ = GetSet::create(&field_ident, &field_ty, &binds, template_ptrs, impls)?;

            Self::handle_two_way_binding(
                &mut twb_poll,
                &binds,
                &field_ident,
                &field_ty,
                &mut impls.traits_impl.0.widget.handle_event,
            )?;
        }
        impls
            .self_ref_impl
            .extend(GetSet::getter_setter(&component_ident));
        // [双向绑定初始化相关的代码] ------------------------------------------------------------------------------
        // 初始化添加到after_apply_from_doc中的初始化双向绑定池的代码
        let twb_poll = TWBPollBuilder(twb_poll);
        let _ = twb_poll.init_tk(component_ident).map(|tk| {
            impls
                .traits_impl
                .0
                .live_hook
                .push(tk, LiveHookType::AfterApplyFromDoc);
        });
        // [处理sugar相关的代码] ---------------------------------------------------------------------------------
        // - [通过tmeplate_ptrs给prop添加组件指针] ----------------------------------------------------------------
        Self::handle_sugar(&mut live_component.0, template_ptrs, impls)?;
        // [添加双向绑定池] --------------------------------------------------------------------------------------
        if twb_poll.is_empty() {
            Ok(None)
        } else {
            Self::append_twb_pool(&mut live_component.0)?;
            Ok(Some(twb_poll))
        }
    }

    pub fn visit_pure(props: Option<&mut Vec<PropItem>>, others: &mut Vec<Stmt>) -> Result<(), Error> {
        if let Some(props) = props {
            Self::props(props, others)?;
        }
        Ok(())
    }

    /// ## params
    /// - component: 使用#[component]修饰的struct
    /// - props: 使用#[prop(bool)]修饰的struct或enum
    /// - binds: 组件和变量之间的绑定关系
    /// - template_ptrs: 组件指针
    /// - impls: 组件的impl
    pub fn visit(
        component: &mut ItemStruct,
        props: Option<&mut Vec<PropItem>>,
        template_ptrs: &TemplatePtrs,
        impls: &mut Impls,
        binds: Option<&Binds>,
        others: &mut Vec<Stmt>,
    ) -> Result<(Option<TWBPollBuilder>, LiveComponent), Error> {
        // [处理props] ------------------------------------------------------------------------------------------
        if let Some(props) = props {
            Self::props(props, others)?;
        }
        // [组件实例初始化] -------------------------------------------------------------------------------------
        let mut live_component = Self::instance(component, impls, binds)?;
        // [生成get和set方法] -----------------------------------------------------------------------------------
        let component_ident = live_component.ident();
        let twb = if let Some(binds) = binds {
            Self::two_way_binding(
                component_ident,
                &mut live_component,
                component,
                binds,
                template_ptrs,
                impls,
            )?
        } else {
            None
        };

        Ok((twb, live_component))
    }

    /// 处理props
    /// 这些props是使用#[prop]修饰的struct或enum，我们需要
    fn props(props: &mut Vec<PropItem>, others: &mut Vec<Stmt>) -> Result<(), Error> {
        for prop_item in props.iter_mut() {
            match prop_item {
                PropItem::Struct(prop) => {
                    // [去除prop宏] -----------------------------------------------------------------------------------
                    prop.attrs.retain(|attr| !attr.path().is_ident("prop"));
                    // [遍历每一个字段并增加#[live]宏] ----------------------------------------------------------------
                    for field in prop.fields.iter_mut() {
                        handle_field_attrs(field)?;
                    }
                    // [添加makepad需要的live宏] -----------------------------------------------------------------------
                    prop.attrs.push(parse_quote! {
                        #[derive(Live, LiveHook, LiveRegister)]
                    });
                    prop.attrs.push(parse_quote! {
                        #[live_ignore]
                    });
                    // [添加到others中] --------------------------------------------------------------------------------
                    others.push(parse_quote! {
                        #prop
                    });
                }
                PropItem::Enum(prop) => {
                    // [去除prop宏] -----------------------------------------------------------------------------------
                    prop.attrs.retain(|attr| !attr.path().is_ident("prop"));
                    // [查找enum上是否使用了#[derive(Default)]来实现Default trait] -----------------------------------
                    // 如果使用了需要去除Default trait
                    let mut has_default_trait = false;
                    let mut derives = prop.attrs.iter().fold(Vec::new(), |mut derives, attr| {
                        if attr.path().is_ident("derive") {
                            let _ = attr.parse_nested_meta(|meta| {
                                if !meta.path.is_ident("Default") {
                                    derives.push(meta.path.to_token_stream());
                                } else {
                                    has_default_trait = true;
                                }
                                Ok(())
                            });
                        }
                        derives
                    });
                    prop.attrs.retain(|attr| !attr.path().is_ident("derive"));
                    // [添加makepad需要的live宏] -----------------------------------------------------------------------
                    derives.extend(vec![
                        quote! {Live},
                        quote! {LiveHook},
                        quote! {LiveRegister},
                    ]);
                    prop.attrs.push(parse_quote! {
                        #[derive(#(#derives),*)]
                    });
                    prop.attrs.push(parse_quote! {
                        #[live_ignore]
                    });
                    // [查找字段上使用使用了#[default]来设置默认字段]-------------------------------------------------------
                    // 如果使用了需要把#[default]改为#[pick], 并生成一个impl Default for 的代码
                    if has_default_trait {
                        let mut pure = false;
                        for var in prop.variants.iter_mut() {
                            // 表示枚举含有字段例如: Name(String), 这样的需要添加`#[live(Default::default())]` (非纯枚举)
                            if !var.fields.is_empty() {
                                var.attrs.push(parse_quote! {
                                    #[live(Default::default())]
                                });
                            } else {
                                // 纯枚举
                                if var.attrs.iter().any(|attr| attr.path().is_ident("default")) {
                                    var.attrs.retain(|attr| !attr.path().is_ident("default"));
                                    var.attrs.push(parse_quote! {
                                        #[pick]
                                    });
                                    let enum_ident = prop.ident.to_token_stream();
                                    let var_ident = var.ident.to_token_stream();
                                    // 控制只生成一次impl Default, 为了以防用户写多个#[default]这不符合rust的语法
                                    if !pure {
                                        others.push(parse_quote! {
                                            impl Default for #enum_ident{
                                                fn default() -> Self{
                                                    Self::#var_ident
                                                }
                                            }
                                        });
                                    }
                                    pure = true;
                                }
                            }
                        }
                    }

                    // [添加到others中] --------------------------------------------------------------------------------
                    others.push(parse_quote! {
                        #prop
                    });
                }
            }
        }

        Ok(())
        // Ok(prop_in_component)
    }

    /// 处理所有双向绑定用到的变量和组件之间的关系，生成添加到handle_event中触发组件事件的代码
    /// 例如：当使用者给checkbox到selected绑定变量时，用户点击checkbox会触发checbox的clicked事件，来更新selected的值
    /// 但实际上用户并没有显示的添加checkbox的@clicked的回调函数，这个回调函数是由双向绑定池自动生成的，属于隐式回调
    fn handle_two_way_binding(
        twb_poll: &mut HashMap<String, String>,
        binds: &Binds,
        field: &str,
        ty: &str,
        handle_event: &mut HandleEvent,
    ) -> Result<(), Error> {
        // 获取使用了字段的所有组件
        if let Some(widgets) = binds.get(field) {
            for widget in widgets {
                let PropComponent { id, name, prop, .. } = widget;
                // 添加到双向绑定池中
                twb_poll.insert(field.to_string(), ty.to_string());
                // 添加到handle_event中触发组件事件的代码
                handle_event
                    .c_refs
                    .insert(CRef::new(id.to_string(), name.to_string()));

                if let Some(event) = BuiltinWidget::twb_event(name, prop.as_str()) {
                    handle_event.callbacks.insert(CallbackStmt::new(
                        id.to_string(),
                        field.to_string(),
                        prop.to_string(),
                        event,
                    ));
                }
            }
        }

        Ok(())
    }

    fn handle_sugar(
        prop: &mut ItemStruct,
        ptrs: &TemplatePtrs,
        impls: &mut Impls,
    ) -> Result<(), Error> {
        SugarScript::visit(prop, ptrs, impls)
    }

    fn append_twb_pool(prop: &mut ItemStruct) -> Result<(), Error> {
        match &mut prop.fields {
            Fields::Named(fields) => {
                let field = TWBPollBuilder::field_token_stream();
                fields.named.push(field);
                return Ok(());
            }
            _ => {
                return Err(CompilerError::runtime(
                    "Makepad Compiler - Script",
                    "prop should be a struct with named fields",
                )
                .into())
            }
        }
    }
}

#[allow(unused)]
#[derive(Debug)]
struct PropInComponent {
    rust: HashSet<String>,
    live: HashSet<String>,
}

#[cfg(test)]
mod tt {
    use quote::ToTokens;
    use syn::parse_quote;

    #[test]
    fn t1() {
        let code = r#"
        #[prop]
        #[derive(Default, Debug)]
        pub enum AProp{
            #[default]
            Name
        }
        "#;

        let mut prop = syn::parse_str::<syn::ItemEnum>(code).unwrap();
        prop.attrs.retain(|attr| !attr.path().is_ident("prop"));
        // [查找enum上是否使用了#[derive(Default)]来实现Default trait] -----------------------------------
        // 如果使用了需要去除Default trait
        let derives = prop.attrs.iter().fold(Vec::new(), |mut derives, attr| {
            if attr.path().is_ident("derive") {
                let _ = attr.parse_nested_meta(|meta| {
                    if !meta.path.is_ident("Default") {
                        derives.push(meta.path.to_token_stream());
                    }
                    Ok(())
                });
            }
            derives
        });
        prop.attrs.retain(|attr| !attr.path().is_ident("derive"));
        prop.attrs.push(parse_quote! {
            #[derive(#(#derives),*)]
        });

        // [查找字段上使用使用了#[default]来设置默认字段]-------------------------------------------------------
        dbg!(prop);
    }
}
