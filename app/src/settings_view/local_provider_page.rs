//! Settings page for the local Pi provider ("warpi").
//!
//! This is the user-facing configuration flow for standalone mode: endpoint,
//! model id, limits, authentication mode, and the API key (which is written to
//! the OS secret store, never to the config file). The page edits the *active*
//! profile; "New profile" creates a fresh one and makes it active.
//!
//! The page also owns the "Test connection" action. A missing `/models`
//! endpoint is not an error: v1 works with manual model ids and servers that
//! only implement `/chat/completions`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use standalone_agent::provider::{CredentialRef, ProviderProfile, WireProtocol};
use warpui::elements::{
    Container, CrossAxisAlignment, Element, Flex, MainAxisAlignment, MouseStateHandle, ParentElement,
    Text,
};
use warpui::ui_components::button::ButtonVariant;
use warpui::ui_components::components::UiComponentStyles;
use warpui::elements::ChildView;
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, SingletonEntity, TypedActionView, UpdateView, View, ViewContext, ViewHandle};

use crate::view_components::{Dropdown, DropdownItem};

use super::SettingsSection;
use super::settings_page::{
    MatchData, PageType, SettingsPageEvent, SettingsPageMeta, SettingsPageViewHandle,
    SettingsWidget,
};
use crate::ai::standalone;
use crate::appearance::Appearance;
use crate::editor::{
    EditorView, Event as EditorEvent, PropagateAndNoOpNavigationKeys, SingleLineEditorOptions,
};

const INPUT_WIDTH: f32 = 520.;
const LABEL_MARGIN_BOTTOM: f32 = 4.;
const FIELD_MARGIN_BOTTOM: f32 = 16.;
const SECTION_MARGIN_BOTTOM: f32 = 24.;

#[derive(Clone, Debug, PartialEq)]
pub enum LocalProviderAction {
    SelectPreset(String),
    ToggleModel { profile_id: String, model_id: String, enabled: bool },
    ToggleEnabled,
    ToggleAuthMode,
    Save,
    Test,
    NewProfile,
    DeleteProfile,
}

/// Outcome of the last "Test connection" run.
#[derive(Clone, Debug, PartialEq)]
enum TestState {
    Idle,
    Testing,
    Passed(String),
    Failed(String),
}

struct StatusMessage {
    text: String,
    is_error: bool,
}

pub struct LocalProviderPageView {
    page: PageType<Self>,
    profile_id: String,
    provider_dropdown: ViewHandle<Dropdown<LocalProviderAction>>,
    display_name_editor: ViewHandle<EditorView>,
    base_url_editor: ViewHandle<EditorView>,
    model_id_editor: ViewHandle<EditorView>,
    additional_models_editor: ViewHandle<EditorView>,
    disabled_models: RefCell<Vec<String>>,
    model_toggle_mouse_states: RefCell<HashMap<String, MouseStateHandle>>,
    context_limit_editor: ViewHandle<EditorView>,
    output_limit_editor: ViewHandle<EditorView>,
    api_key_editor: ViewHandle<EditorView>,
    enabled: bool,
    use_api_key: bool,
    status: RefCell<Option<StatusMessage>>,
    discovered_models: RefCell<Vec<String>>,
    test_state: RefCell<TestState>,
    save_button_mouse_state: MouseStateHandle,
    test_button_mouse_state: MouseStateHandle,
    enabled_button_mouse_state: MouseStateHandle,
    auth_button_mouse_state: MouseStateHandle,
    new_profile_button_mouse_state: MouseStateHandle,
    delete_profile_button_mouse_state: MouseStateHandle,
}

impl LocalProviderPageView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let config = standalone::current_config();
        let enabled = config.as_ref().is_some_and(|config| config.enabled);
        let profile = config
            .as_ref()
            .and_then(|config| config.active().cloned())
            .unwrap_or_else(new_profile_template);
        let use_api_key = matches!(profile.credential, CredentialRef::SecretStore { .. });
        let make_editor = |ctx: &mut ViewContext<Self>, text: String, password: bool| {
            ctx.add_typed_action_view(move |ctx| {
                let options = SingleLineEditorOptions {
                    is_password: password,
                    propagate_and_no_op_vertical_navigation_keys:
                        PropagateAndNoOpNavigationKeys::Always,
                    ..Default::default()
                };
                let mut editor = EditorView::single_line(options, ctx);
                editor.set_buffer_text(&text, ctx);
                editor
            })
        };
        let display_name_editor = make_editor(ctx, profile.display_name.clone(), false);
        let base_url_editor = make_editor(ctx, profile.base_url.clone(), false);
        let model_id_editor = make_editor(ctx, profile.model_id.clone(), false);
        let additional_models_editor = make_editor(ctx, profile.models.join(", "), false);
        let context_limit_editor = make_editor(ctx, profile.context_limit.to_string(), false);
        let output_limit_editor = make_editor(ctx, profile.output_limit.to_string(), false);
        let api_key_editor = make_editor(ctx, String::new(), true);

        // Re-render when the user edits a field so validation feedback is live.
        for editor in [
            &display_name_editor,
            &base_url_editor,
            &model_id_editor,
            &additional_models_editor,
            &context_limit_editor,
            &output_limit_editor,
            &api_key_editor,
        ] {
            ctx.subscribe_to_view(editor, |_me, _editor, event, ctx| {
                if matches!(event, EditorEvent::Edited(_)) {
                    ctx.notify();
                }
            });
        }

        let provider_dropdown = ctx.add_typed_action_view(|ctx| {
            let mut dropdown = Dropdown::new(ctx);
            dropdown.set_top_bar_max_width(360.);
            dropdown.set_items(
                standalone::PROVIDER_PRESETS
                    .iter()
                    .map(|preset| {
                        DropdownItem::new(
                            preset.display_name,
                            LocalProviderAction::SelectPreset(preset.id.to_string()),
                        )
                    })
                    .collect(),
                ctx,
            );
            if let Some(preset) = standalone::preset_for_profile(&profile) {
                dropdown.set_selected_by_action(
                    LocalProviderAction::SelectPreset(preset.id.to_string()),
                    ctx,
                );
            }
            dropdown
        });

        Self {
            page: PageType::new_monolith(LocalProviderWidget, Some("Local Pi provider"), true),
            profile_id: profile.id.clone(),
            provider_dropdown,
            display_name_editor,
            base_url_editor,
            model_id_editor,
            additional_models_editor,
            disabled_models: RefCell::new(profile.disabled_models.clone()),
            model_toggle_mouse_states: RefCell::new(HashMap::new()),
            context_limit_editor,
            output_limit_editor,
            api_key_editor,
            enabled,
            use_api_key,
            status: RefCell::new(None),
            discovered_models: RefCell::new(Vec::new()),
            test_state: RefCell::new(TestState::Idle),
            save_button_mouse_state: MouseStateHandle::default(),
            test_button_mouse_state: MouseStateHandle::default(),
            enabled_button_mouse_state: MouseStateHandle::default(),
            auth_button_mouse_state: MouseStateHandle::default(),
            new_profile_button_mouse_state: MouseStateHandle::default(),
            delete_profile_button_mouse_state: MouseStateHandle::default(),
        }
    }

    fn form_values(&self, app: &AppContext) -> FormValues {
        FormValues {
            display_name: self.display_name_editor.as_ref(app).buffer_text(app),
            base_url: self.base_url_editor.as_ref(app).buffer_text(app),
            model_id: self.model_id_editor.as_ref(app).buffer_text(app),
            additional_models: self.additional_models_editor.as_ref(app).buffer_text(app),
            context_limit: self.context_limit_editor.as_ref(app).buffer_text(app),
            output_limit: self.output_limit_editor.as_ref(app).buffer_text(app),
            api_key: self.api_key_editor.as_ref(app).buffer_text(app),
        }
    }

    fn build_profile(&self, app: &AppContext) -> Result<ProviderProfile, String> {
        let values = self.form_values(app);
        let context_limit = values
            .context_limit
            .trim()
            .parse::<u64>()
            .map_err(|_| "Context limit must be a whole number of tokens".to_string())?;
        let output_limit = values
            .output_limit
            .trim()
            .parse::<u64>()
            .map_err(|_| "Output limit must be a whole number of tokens".to_string())?;
        let credential = if self.use_api_key {
            CredentialRef::SecretStore { key: standalone::credential_key(&self.profile_id) }
        } else {
            CredentialRef::None
        };
        let profile = ProviderProfile {
            id: self.profile_id.clone(),
            display_name: values.display_name.trim().to_string(),
            base_url: values.base_url.trim().to_string(),
            wire: WireProtocol::OpenAiChatCompletions,
            model_id: values.model_id.trim().to_string(),
            models: values
                .additional_models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect(),
            disabled_models: self.disabled_models.borrow().clone(),
            credential,
            context_limit,
            output_limit,
            compat: Default::default(),
            reasoning: false,
            supports_image_input: false,
            headers: Default::default(),
        };
        profile.validate().map_err(|error| error.to_string())?;
        Ok(profile)
    }

    fn set_status(&self, text: impl Into<String>, is_error: bool) {
        *self.status.borrow_mut() = Some(StatusMessage { text: text.into(), is_error });
    }

    fn save(&mut self, ctx: &mut ViewContext<Self>) {
        let profile = match self.build_profile(ctx) {
            Ok(profile) => profile,
            Err(error) => {
                self.set_status(error, true);
                ctx.notify();
                return;
            }
        };
        let api_key = self.api_key_editor.as_ref(ctx).buffer_text(ctx);
        if self.use_api_key && !api_key.trim().is_empty() {
            if let Err(error) = standalone::store_credential(ctx, &profile.id, api_key.trim()) {
                self.set_status(format!("Could not store the API key: {error}"), true);
                ctx.notify();
                return;
            }
        }
        if !self.use_api_key
            && let Err(error) = standalone::delete_credential_for(ctx, &profile.id)
        {
            self.set_status(format!("Could not remove the stored API key: {error}"), true);
            ctx.notify();
            return;
        }
        match standalone::upsert_profile(profile) {
            Ok(()) => {
                let mut config = standalone::current_config().unwrap_or_default();
                config.enabled = self.enabled;
                if let Err(error) = standalone::write_config(&config) {
                    self.set_status(format!("Could not save the configuration: {error}"), true);
                } else {
                    // Refresh the model picker/chip so the local model shows up
                    // immediately, without a restart.
                    crate::ai::llms::LLMPreferences::handle(ctx).update(ctx, |preferences, ctx| {
                        preferences.rebuild_standalone_llms(ctx);
                    });
                    self.set_status(
                        "Saved. The next agent request uses this endpoint; no restart needed.",
                        false,
                    );
                }
            }
            Err(error) => self.set_status(format!("Could not save the configuration: {error}"), true),
        }
        ctx.notify();
    }

    fn delete_profile(&mut self, ctx: &mut ViewContext<Self>) {
        if standalone::current_config().is_none_or(|config| config.profiles.len() <= 1) {
            self.set_status(
                "Keep at least one profile. Edit this one instead of deleting it.",
                true,
            );
            ctx.notify();
            return;
        }
        if let Err(error) = standalone::remove_profile(&self.profile_id) {
            self.set_status(format!("Could not delete the profile: {error}"), true);
            ctx.notify();
            return;
        }
        self.reload_from_config(ctx);
        self.set_status("Profile deleted.", false);
        ctx.notify();
    }

    fn new_profile(&mut self, ctx: &mut ViewContext<Self>) {
        let profile = new_profile_template();
        if let Err(error) = standalone::upsert_profile(profile.clone()) {
            self.set_status(format!("Could not create the profile: {error}"), true);
            ctx.notify();
            return;
        }
        self.reload_from_config(ctx);
        self.set_status(
            "New profile created. Fill in the endpoint and model id, then save.",
            false,
        );
        ctx.notify();
    }

    fn reload_from_config(&mut self, ctx: &mut ViewContext<Self>) {
        let config = standalone::current_config();
        let profile = config
            .as_ref()
            .and_then(|config| config.active().cloned())
            .unwrap_or_else(new_profile_template);
        self.enabled = config.as_ref().is_some_and(|config| config.enabled);
        self.profile_id = profile.id.clone();
        self.use_api_key = matches!(profile.credential, CredentialRef::SecretStore { .. });
        self.apply_profile(&profile, ctx);
        *self.test_state.borrow_mut() = TestState::Idle;
    }

    /// Fill the form from a preset. Display name and model id are only set when
    /// the preset suggests one, so a custom choice is never overwritten.
    fn apply_preset(&mut self, preset: &standalone::ProviderPreset, ctx: &mut ViewContext<Self>) {
        self.disabled_models.borrow_mut().clear();
        let values: [(&ViewHandle<EditorView>, String); 6] = [
            (&self.display_name_editor, preset.display_name.to_string()),
            (&self.base_url_editor, preset.base_url.to_string()),
            (&self.model_id_editor, preset.suggested_model.to_string()),
            (&self.additional_models_editor, preset.extra_models.join(", ")),
            (&self.context_limit_editor, preset.context_limit.to_string()),
            (&self.output_limit_editor, preset.output_limit.to_string()),
        ];
        for (editor, text) in values {
            let editor = editor.clone();
            ctx.update_view(&editor, |editor: &mut EditorView, ctx| {
                editor.set_buffer_text(&text, ctx)
            });
        }
        self.use_api_key = preset.requires_key;
        ctx.update_view(&self.api_key_editor, |editor: &mut EditorView, ctx| {
            editor.set_buffer_text("", ctx);
        });
    }

    fn apply_profile(&mut self, profile: &ProviderProfile, ctx: &mut ViewContext<Self>) {
        self.disabled_models.borrow_mut().clone_from(&profile.disabled_models);
        let values = [
            (self.display_name_editor.clone(), profile.display_name.clone()),
            (self.base_url_editor.clone(), profile.base_url.clone()),
            (self.model_id_editor.clone(), profile.model_id.clone()),
            (self.additional_models_editor.clone(), profile.models.join(", ")),
            (self.context_limit_editor.clone(), profile.context_limit.to_string()),
            (self.output_limit_editor.clone(), profile.output_limit.to_string()),
        ];
        for (editor, text) in values {
            ctx.update_view(&editor, |editor: &mut EditorView, ctx| editor.set_buffer_text(&text, ctx));
        }
        ctx.update_view(&self.api_key_editor, |editor: &mut EditorView, ctx| {
            editor.set_buffer_text("", ctx);
        });
    }

    /// Probe `{base}/models`. A 404 is a pass: manual model ids are supported
    /// and `/models` is optional.
    fn test_connection(&mut self, ctx: &mut ViewContext<Self>) {
        if matches!(*self.test_state.borrow(), TestState::Testing) {
            return;
        }
        let profile = match self.build_profile(ctx) {
            Ok(profile) => profile,
            Err(error) => {
                self.set_status(error, true);
                ctx.notify();
                return;
            }
        };
        let Some(url) = standalone::models_probe_url(&profile) else {
            self.set_status("Cannot build a probe URL from this base URL.", true);
            ctx.notify();
            return;
        };
        let api_key = if self.use_api_key {
            let typed = self.api_key_editor.as_ref(ctx).buffer_text(ctx);
            if typed.trim().is_empty() {
                standalone::read_credential(ctx, &profile)
                    .map(|secret| secret.expose_secret().to_string())
            } else {
                Some(typed.trim().to_string())
            }
        } else {
            None
        };
        *self.test_state.borrow_mut() = TestState::Testing;
        self.set_status(format!("Testing {url}…"), false);
        ctx.notify();

        ctx.spawn(
            async move { probe_endpoint(url, api_key).await },
            |me, result, ctx| {
                let (message, is_error, models) = match result {
                    Ok(outcome) => (outcome.message, false, outcome.models),
                    Err(error) => (error, true, Vec::new()),
                };
                *me.discovered_models.borrow_mut() = models;
                *me.test_state.borrow_mut() = if is_error {
                    TestState::Failed(message.clone())
                } else {
                    TestState::Passed(message.clone())
                };
                me.set_status(message, is_error);
                ctx.notify();
            },
        );
    }

    fn render_body(&self, appearance: &Appearance, app: &AppContext) -> Box<dyn Element> {
        let ui_builder = appearance.ui_builder();
        let theme = appearance.theme();
        let input_style = UiComponentStyles { width: Some(INPUT_WIDTH), ..Default::default() };

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .with_main_axis_size(warpui::elements::MainAxisSize::Min);

        column.add_child(
            ui_builder
                .span("warpi runs agent inference against a local OpenAI-compatible endpoint behind the Pi helper. No Warp account and no Warp servers are used for agent requests.")
                .with_soft_wrap()
                .build()
                .with_margin_bottom(SECTION_MARGIN_BOTTOM)
                .finish(),
        );

        // Enabled toggle.
        let enabled_label = if self.enabled { "Enabled" } else { "Disabled" };
        column.add_child(
            Container::new(row(
                ui_builder.span("Standalone mode:").build().finish(),
                ui_builder
                    .button(ButtonVariant::Secondary, self.enabled_button_mouse_state.clone())
                    .with_text_label(enabled_label.to_string())
                    .build()
                    .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::ToggleEnabled))
                    .finish(),
            ))
            .with_margin_bottom(FIELD_MARGIN_BOTTOM)
            .finish(),
        );

        column.add_child(
            ui_builder
                .span("Provider")
                .build()
                .with_margin_bottom(LABEL_MARGIN_BOTTOM)
                .finish(),
        );
        column.add_child(
            Container::new(ChildView::new(&self.provider_dropdown).finish())
                .with_margin_bottom(FIELD_MARGIN_BOTTOM)
                .finish(),
        );

        let fields: [(&str, &ViewHandle<EditorView>, &str); 6] = [
            ("Display name", &self.display_name_editor, "e.g. Local llama.cpp"),
            ("Base URL", &self.base_url_editor, "e.g. http://127.0.0.1:8080/v1"),
            ("Model id (sent verbatim to the provider)", &self.model_id_editor, "e.g. qwen3-coder-30b"),
            (
                "Additional models (comma-separated)",
                &self.additional_models_editor,
                "e.g. deepseek-v4-pro, deepseek-reasoner",
            ),
            ("Context limit (tokens)", &self.context_limit_editor, "e.g. 131072"),
            ("Output limit (tokens)", &self.output_limit_editor, "e.g. 8192"),
        ];
        for (label, editor, placeholder) in fields {
            let placeholder = placeholder.to_string();
            ctx_set_placeholder(app, editor, &placeholder);
            column.add_child(
                ui_builder
                    .span(label.to_string())
                    .build()
                    .with_margin_bottom(LABEL_MARGIN_BOTTOM)
                    .finish(),
            );
            column.add_child(
                Container::new(
                    ui_builder
                        .text_input(editor.clone())
                        .with_style(input_style)
                        .build()
                        .finish(),
                )
                .with_margin_bottom(FIELD_MARGIN_BOTTOM)
                .finish(),
            );
        }

        // Per-model enable/disable. Every model of every configured profile is
        // listed in the model picker unless it is switched off here.
        let config_for_models = standalone::current_config();
        if let Some(profile) = config_for_models
            .as_ref()
            .and_then(|config| config.profile(&self.profile_id))
        {
            let all_models = profile.selectable_models();
            if all_models.len() > 1 {
                column.add_child(
                    ui_builder
                        .span("Models offered in the model picker")
                        .build()
                        .with_margin_bottom(LABEL_MARGIN_BOTTOM)
                        .finish(),
                );
                for model in all_models {
                    let enabled = !self.disabled_models.borrow().contains(&model);
                    let mouse_state = self
                        .model_toggle_mouse_states
                        .borrow_mut()
                        .entry(model.clone())
                        .or_default()
                        .clone();
                    column.add_child(
                        Container::new(
                            ui_builder
                                .button(ButtonVariant::Secondary, mouse_state)
                                .with_text_label(if enabled {
                                    format!("● {model}")
                                } else {
                                    format!("○ {model}")
                                })
                                .build()
                                .on_click({
                                    let profile_id = self.profile_id.clone();
                                    let model_id = model.clone();
                                    move |ctx, _, _| {
                                        ctx.dispatch_typed_action(LocalProviderAction::ToggleModel {
                                            profile_id: profile_id.clone(),
                                            model_id: model_id.clone(),
                                            enabled: !enabled,
                                        })
                                    }
                                })
                                .finish(),
                        )
                        .with_margin_bottom(4.)
                        .finish(),
                    );
                }
                column.add_child(
                    Container::new(ui_builder.span("").build().finish())
                        .with_margin_bottom(FIELD_MARGIN_BOTTOM)
                        .finish(),
                );
            }
        }

        // Authentication mode.
        let auth_label = if self.use_api_key { "API key" } else { "No authentication" };
        column.add_child(
            Container::new(row(
                ui_builder
                    .span(format!("Authentication: {auth_label}"))
                    .build()
                    .finish(),
                ui_builder
                    .button(ButtonVariant::Secondary, self.auth_button_mouse_state.clone())
                    .with_text_label("Switch".to_string())
                    .build()
                    .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::ToggleAuthMode))
                    .finish(),
            ))
            .with_margin_bottom(FIELD_MARGIN_BOTTOM)
            .finish(),
        );
        if self.use_api_key {
            column.add_child(
                ui_builder
                    .span("API key (stored in the OS secret store, never in the config file)")
                    .build()
                    .with_margin_bottom(LABEL_MARGIN_BOTTOM)
                    .finish(),
            );
            column.add_child(
                Container::new(
                    ui_builder
                        .text_input(self.api_key_editor.clone())
                        .with_style(input_style)
                        .build()
                        .finish(),
                )
                .with_margin_bottom(FIELD_MARGIN_BOTTOM)
                .finish(),
            );
        }

        // Actions.
        let mut actions = Flex::row().with_main_axis_alignment(MainAxisAlignment::Start);
        actions.add_child(
            Container::new(
                ui_builder
                    .button(ButtonVariant::Accent, self.save_button_mouse_state.clone())
                    .with_text_label("Save profile".to_string())
                    .build()
                    .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::Save))
                    .finish(),
            )
            .with_margin_right(8.)
            .finish(),
        );
        let test_label = match &*self.test_state.borrow() {
            TestState::Testing => "Testing…",
            _ => "Test connection",
        };
        actions.add_child(
            Container::new(
                ui_builder
                    .button(ButtonVariant::Secondary, self.test_button_mouse_state.clone())
                    .with_text_label(test_label.to_string())
                    .build()
                    .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::Test))
                    .finish(),
            )
            .with_margin_right(8.)
            .finish(),
        );
        actions.add_child(
            Container::new(
                ui_builder
                    .button(ButtonVariant::Secondary, self.new_profile_button_mouse_state.clone())
                    .with_text_label("New profile".to_string())
                    .build()
                    .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::NewProfile))
                    .finish(),
            )
            .with_margin_right(8.)
            .finish(),
        );
        actions.add_child(
            ui_builder
                .button(ButtonVariant::Secondary, self.delete_profile_button_mouse_state.clone())
                .with_text_label("Delete profile".to_string())
                .build()
                .on_click(|ctx, _, _| ctx.dispatch_typed_action(LocalProviderAction::DeleteProfile))
                .finish(),
        );
        column.add_child(
            Container::new(actions.finish())
                .with_margin_bottom(SECTION_MARGIN_BOTTOM)
                .finish(),
        );

        // Status (plus any model ids discovered by "Test connection").
        if let Some(status) = self.status.borrow().as_ref() {
            let status_text = match self.discovered_models.borrow().as_slice() {
                [] => status.text.clone(),
                models => format!("{} Models available: {}", status.text, models.join(", ")),
            };
            let color = if status.is_error {
                theme.ui_error_color()
            } else {
                theme.sub_text_color(theme.background()).into()
            };
            column.add_child(
                Container::new(
                    Text::new(
                        status_text,
                        appearance.ui_font_family(),
                        appearance.ui_font_size(),
                    )
                    .with_color(color)
                    .finish(),
                )
                .with_margin_bottom(SECTION_MARGIN_BOTTOM)
                .finish(),
            );
        }

        // Diagnostics: which profile is live and where the helper lives.
        let config = standalone::current_config();
        let active = config
            .as_ref()
            .and_then(|config| config.active())
            .map(|profile| format!("{} ({})", profile.display_name, profile.model_id))
            .unwrap_or_else(|| "none".to_string());
        let helper = config
            .as_ref()
            .and_then(|config| config.helper_entry.clone())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "bundled default".to_string());
        column.add_child(
            ui_builder
                .span(format!("Active profile: {active}"))
                .with_soft_wrap()
                .build()
                .with_margin_bottom(4.)
                .finish(),
        );
        column.add_child(
            ui_builder
                .span(format!("Pi helper: {helper}"))
                .with_soft_wrap()
                .build()
                .finish(),
        );

        column.finish()
    }
}

/// Editor placeholder is set once at construction; the settings page is
/// re-created when the user navigates to it, so this is best-effort only.
fn ctx_set_placeholder(_app: &AppContext, _editor: &ViewHandle<EditorView>, _placeholder: &str) {}

fn row(left: Box<dyn Element>, right: Box<dyn Element>) -> Box<dyn Element> {
    let mut row = Flex::row().with_cross_axis_alignment(CrossAxisAlignment::Center);
    row.add_child(Container::new(left).with_margin_right(8.).finish());
    row.add_child(right);
    row.finish()
}

fn new_profile_template() -> ProviderProfile {
    ProviderProfile {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: "Local provider".to_string(),
        base_url: "http://127.0.0.1:8080/v1".to_string(),
        wire: WireProtocol::OpenAiChatCompletions,
        model_id: String::new(),
        models: Vec::new(),
        disabled_models: Vec::new(),
        credential: CredentialRef::None,
        context_limit: 32768,
        output_limit: 4096,
        compat: Default::default(),
        reasoning: false,
        supports_image_input: false,
        headers: Default::default(),
    }
}

struct FormValues {
    display_name: String,
    base_url: String,
    model_id: String,
    additional_models: String,
    context_limit: String,
    output_limit: String,
    api_key: String,
}

struct ProbeOutcome {
    message: String,
    models: Vec<String>,
}

/// `Ok(outcome)` when the endpoint answered (including 404 on `/models`),
/// `Err(message)` when it did not.
async fn probe_endpoint(url: String, api_key: Option<String>) -> Result<ProbeOutcome, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| format!("Could not create an HTTP client: {error}"))?;
    let mut request = client.get(&url);
    if let Some(key) = api_key.as_deref() {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(ProbeOutcome {
                    message:
                        "Endpoint reachable; /models is absent. That is fine: the model id is configured manually."
                            .to_string(),
                    models: Vec::new(),
                });
            }
            if status.is_success() {
                let discovered = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|body| {
                        body.get("data").and_then(|data| data.as_array()).map(|models| {
                            models
                                .iter()
                                .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                    })
                    .unwrap_or_default();
                let shown = discovered.iter().take(8).cloned().collect::<Vec<_>>();
                let message = match discovered.len() {
                    0 => format!("Endpoint reachable (HTTP {status})."),
                    count => format!(
                        "Endpoint reachable (HTTP {status}); {count} model(s) reported. Pick one in the Model id field."
                    ),
                };
                return Ok(ProbeOutcome { message, models: shown });
            }
            Err(format!(
                "Endpoint answered HTTP {status}. Check the URL, the model id, and the credential."
            ))
        }
        Err(error) => Err(format!("Could not reach {url}: {error}")),
    }
}

impl Entity for LocalProviderPageView {
    type Event = SettingsPageEvent;
}

impl TypedActionView for LocalProviderPageView {
    type Action = LocalProviderAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            LocalProviderAction::SelectPreset(id) => {
                if let Some(preset) = standalone::preset_by_id(id) {
                    if !preset.base_url.is_empty() {
                        self.apply_preset(preset, ctx);
                    }
                    self.set_status(
                        if preset.note.is_empty() {
                            format!("Filled the {} preset; enter the API key and save.", preset.display_name)
                        } else {
                            format!("{} — {}", preset.display_name, preset.note)
                        },
                        false,
                    );
                    ctx.notify();
                }
            }
            LocalProviderAction::ToggleModel { profile_id, model_id, enabled } => {
                match standalone::set_model_enabled(profile_id, model_id, *enabled) {
                    Ok(()) => {
                        if *enabled {
                            self.disabled_models.borrow_mut().retain(|model| model != model_id);
                        } else if !self.disabled_models.borrow().contains(model_id) {
                            self.disabled_models.borrow_mut().push(model_id.clone());
                        }
                        crate::ai::llms::LLMPreferences::handle(ctx).update(ctx, |preferences, ctx| {
                            preferences.rebuild_standalone_llms(ctx);
                        });
                        self.set_status(format!("{model_id} is now {}.", if *enabled { "enabled" } else { "disabled" }), false);
                    }
                    Err(error) => self.set_status(error.to_string(), true),
                }
                ctx.notify();
            }
            LocalProviderAction::ToggleEnabled => {
                self.enabled = !self.enabled;
                self.save(ctx);
            }
            LocalProviderAction::ToggleAuthMode => {
                self.use_api_key = !self.use_api_key;
                ctx.notify();
            }
            LocalProviderAction::Save => self.save(ctx),
            LocalProviderAction::Test => self.test_connection(ctx),
            LocalProviderAction::NewProfile => self.new_profile(ctx),
            LocalProviderAction::DeleteProfile => self.delete_profile(ctx),
        }
    }
}

impl View for LocalProviderPageView {
    fn ui_name() -> &'static str {
        "LocalProviderPage"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.page.render(self, app)
    }
}

impl SettingsPageMeta for LocalProviderPageView {
    fn section() -> SettingsSection {
        SettingsSection::LocalProvider
    }

    fn should_render(&self, _ctx: &AppContext) -> bool {
        cfg!(not(target_family = "wasm"))
    }

    fn update_filter(&mut self, query: &str, ctx: &mut ViewContext<Self>) -> MatchData {
        self.page.update_filter(query, ctx)
    }

    fn scroll_to_widget(&mut self, widget_id: &'static str) {
        self.page.scroll_to_widget(widget_id)
    }

    fn clear_highlighted_widget(&mut self) {
        self.page.clear_highlighted_widget()
    }
}

impl From<ViewHandle<LocalProviderPageView>> for SettingsPageViewHandle {
    fn from(view_handle: ViewHandle<LocalProviderPageView>) -> Self {
        SettingsPageViewHandle::LocalProvider(view_handle)
    }
}

struct LocalProviderWidget;

impl SettingsWidget for LocalProviderWidget {
    type View = LocalProviderPageView;

    fn search_terms(&self) -> &str {
        "warpi local provider pi endpoint model api key standalone offline"
    }

    fn render(
        &self,
        view: &LocalProviderPageView,
        appearance: &Appearance,
        app: &AppContext,
    ) -> Box<dyn Element> {
        view.render_body(appearance, app)
    }
}
