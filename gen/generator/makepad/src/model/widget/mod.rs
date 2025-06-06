mod abs;
mod handler;
pub mod role;
mod template;
mod traits;

use std::sync::{Arc, RwLock};

pub use abs::*;
pub use handler::*;
pub use template::*;
pub use traits::*;

use crate::{
    compiler::Context,
    token::{import_default_all, import_draw_shader, use_crate_all, use_default_all},
};
use gen_analyzer::{Model, Polls, Script, Style, Template};
use gen_utils::{common::Source, compiler::ToRs, error::Error};
use quote::{quote, ToTokens};

#[derive(Debug, Clone)]
pub struct Widget {
    pub source: Source,
    pub is_entry: bool,
    pub template: Option<WidgetTemplate>,
    pub template_ptrs: Option<Vec<WidgetTemplate>>,
    pub script: Option<crate::script::Script>,
    pub has_plugin: bool,
}

impl Widget {
    pub fn new(context: &mut Context, model: Model) -> Result<Self, Error> {
        let is_entry = model.is_entry;
        if is_entry {
            context
                .app_main
                .root_ref
                .source
                .replace(model.special.clone());
        }

        let widget = Widget::try_from((context, model))?;
        Ok(widget)
    }
    pub fn imports(&self) -> Option<proc_macro2::TokenStream> {
        self.script.as_ref().and_then(|sc| sc.uses())
    }
    /// default script impl for easy define widget
    pub fn patch_or_default_script(&mut self) -> Result<(), Error> {
        // 确保有template
        if let Some(template) = self.template.as_ref() {
            if let Some(patch_sc) = self.script.as_ref() {
                if let crate::script::Script::Rust(patch_sc) = patch_sc {
                    if patch_sc.live_component.is_some() {
                        return Ok(());
                    }
                    // 说明没有进行具体的定义，但有一些其他的代码，需要patch
                    self.script = template
                        .is_define_root_and(|define_widget| {
                            let mut script = define_widget.default_script();
                            script.patch(patch_sc);
                            Ok::<crate::script::Script, Error>(script)
                        })
                        .transpose()?;
                }
            } else {
                self.script = template
                    .is_define_root_and(|define_widget| {
                        Ok::<crate::script::Script, Error>(define_widget.default_script())
                    })
                    .transpose()?;
            }
        }

        Ok(())
    }
    pub fn uses_token_stream(&self) -> proc_macro2::TokenStream {
        let mut tk = use_default_all();
        if self.has_plugin {
            tk.extend(use_crate_all());
        }
        tk
    }
    /// 获取script的token_stream
    pub fn script_token_stream(&self) -> Option<proc_macro2::TokenStream> {
        self.script.as_ref().map(|sc| sc.to_token_stream())
    }

    pub fn is_empty(&self) -> bool {
        self.template.is_none() && self.script.is_none()
    }

    pub fn is_global(&self) -> bool {
        if let Some(template) = self.template.as_ref() {
            template.is_global()
        } else {
            false
        }
    }
}

impl TryFrom<(&mut Context, Model)> for Widget {
    type Error = Error;

    fn try_from(value: (&mut Context, Model)) -> Result<Self, Self::Error> {
        let (context, model) = value;
        // [分析Model策略] ------------------------------------------------------------------------------------
        let Model {
            special,
            template,
            script,
            style,
            is_entry,
            strategy,
            polls,
            ..
        } = model;

        // [handle commons] ----------------------------------------------------------------------------------

        let widget = match strategy {
            gen_analyzer::Strategy::SingleStyle => (special, style, is_entry).try_into(),
            gen_analyzer::Strategy::SingleTemplate => (special, template, is_entry).try_into(),
            gen_analyzer::Strategy::SingleScript => (context, special, script, is_entry).try_into(),
            gen_analyzer::Strategy::TemplateScript => {
                (context, special, template, script, is_entry, polls).try_into()
            }
            gen_analyzer::Strategy::TemplateStyle => {
                (special, template, style, is_entry).try_into()
            }
            gen_analyzer::Strategy::All => {
                (context, special, template, script, style, is_entry, polls).try_into()
            }
            gen_analyzer::Strategy::None => (special, is_entry).try_into(), // means no strategy, just a empty file
            _ => panic!("can not reach here"),
        }?;

        Ok(widget)
    }
}

/// 解析空文件
impl TryFrom<(Source, bool)> for Widget {
    type Error = Error;

    fn try_from(value: (Source, bool)) -> Result<Self, Self::Error> {
        let (source, is_entry) = value;
        Ok(Widget {
            source,
            is_entry,
            template: None,
            script: None,
            has_plugin: false,
            template_ptrs: None,
        })
    }
}

/// 解析单style模版
/// 处理只有单个<style>标签的情况, 这种情况需要将style转为Makepad的Global Prop即可
impl TryFrom<(Source, Option<Style>, bool)> for Widget {
    type Error = Error;

    fn try_from(value: (Source, Option<Style>, bool)) -> Result<Self, Self::Error> {
        handler::single_style(value.0, value.1, value.2)
    }
}

/// 解析单template模版
impl TryFrom<(Source, Option<Template>, bool)> for Widget {
    type Error = Error;

    fn try_from(value: (Source, Option<Template>, bool)) -> Result<Self, Self::Error> {
        handler::single_template(value.0, value.1, value.2)
    }
}

/// 解析单script模版
/// 处理只有单个<script>标签的情况,
impl TryFrom<(&mut Context, Source, Option<Script>, bool)> for Widget {
    type Error = Error;

    fn try_from(value: (&mut Context, Source, Option<Script>, bool)) -> Result<Self, Self::Error> {
        handler::single_script(value.0, value.1, value.2, value.3)
    }
}

/// 解析template + style模版
impl TryFrom<(Source, Option<Template>, Option<Style>, bool)> for Widget {
    type Error = Error;

    fn try_from(
        value: (Source, Option<Template>, Option<Style>, bool),
    ) -> Result<Self, Self::Error> {
        handler::template_style(value.0, value.1, value.2, value.3)
    }
}

/// 解析template + script模版
impl
    TryFrom<(
        &mut Context,
        Source,
        Option<Template>,
        Option<Script>,
        bool,
        Arc<RwLock<Polls>>,
    )> for Widget
{
    type Error = Error;

    fn try_from(
        value: (
            &mut Context,
            Source,
            Option<Template>,
            Option<Script>,
            bool,
            Arc<RwLock<Polls>>,
        ),
    ) -> Result<Self, Self::Error> {
        handler::template_script(value.0, value.1, value.2, value.3, value.4, value.5)
    }
}

/// 解析template + script + style模版
impl
    TryFrom<(
        &mut Context,
        Source,
        Option<Template>,
        Option<Script>,
        Option<Style>,
        bool,
        Arc<RwLock<Polls>>,
    )> for Widget
{
    type Error = Error;

    fn try_from(
        value: (
            &mut Context,
            Source,
            Option<Template>,
            Option<Script>,
            Option<Style>,
            bool,
            Arc<RwLock<Polls>>,
        ),
    ) -> Result<Self, Self::Error> {
        handler::all(
            value.0, value.1, value.2, value.3, value.4, value.5, value.6,
        )
    }
}

// 实现最终的ToRs，将Widget最终能够输出为rs文件
impl ToRs for Widget {
    fn source(&self) -> Option<&Source> {
        Some(&self.source)
    }

    fn content(&self) -> Result<proc_macro2::TokenStream, Error> {
        let mut tokens = proc_macro2::TokenStream::new();
        // [如果是空文件, 直接返回] ----------------------------------------------------------------------------
        if self.is_empty() {
            return Ok(quote! {});
        }

        // [template] ---------------------------------------------------------------------------------------
        let template = if let Some(template) = self.template.as_ref() {
            // [引入依赖] -------------------------------------------------------------------------------------
            let uses = self.uses_token_stream();
            // [引入Makepad的全局依赖] -------------------------------------------------------------------------
            let mut imports = if self.is_global() {
                import_draw_shader()
            } else {
                import_default_all()
            };
            let component_imports = self.imports();
            if let Some(tk) = component_imports.as_ref() {
                imports.extend(tk.clone());
            }

            let template = template.to_token_stream(self.template_ptrs.as_ref())?;
            let pub_sign = if template.is_empty() {
                None
            } else {
                Some(quote! {pub})
            };

            Some(quote! {
                #uses
                #component_imports
                live_design!{
                    #imports
                    #pub_sign #template
                }
            })
        } else {
            None
        };
        // [script] -----------------------------------------------------------------------------------------
        let script = self.script_token_stream();
        // [合并] --------------------------------------------------------------------------------------------
        tokens.extend(quote! {
            #template
            #script
        });

        Ok(tokens)
    }
}
