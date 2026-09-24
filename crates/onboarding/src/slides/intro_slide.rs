use pathfinder_color::ColorU;
use pathfinder_geometry::vector::vec2f;
use ui_components::{Component as _, Options as _, button};
use warp_core::features::FeatureFlag;
use warp_core::send_telemetry_from_ctx;
use warp_core::ui::Icon;
use warp_core::ui::appearance::Appearance;
use warp_core::ui::theme::ColorScheme;
use warp_core::ui::theme::color::internal_colors;
use warpui_core::assets::asset_cache::AssetSource;
use warpui_core::elements::shimmering_text::{
    ShimmerConfig, ShimmeringTextElement, ShimmeringTextStateHandle,
};
use warpui_core::elements::{
    Align, CacheOption, ChildAnchor, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment,
    Flex, FormattedTextElement, Image, MainAxisAlignment, MainAxisSize, MouseStateHandle,
    OffsetPositioning, ParentAnchor, ParentElement, ParentOffsetBounds, Radius, Stack,
};
use warpui_core::fonts::Weight;
use warpui_core::keymap::Keystroke;
use warpui_core::text_layout::TextAlignment;
use warpui_core::ui_components::components::{UiComponent as _, UiComponentStyles};
use warpui_core::{
    AppContext, Element, Entity, ModelHandle, SingletonEntity as _, TypedActionView, View,
    ViewContext,
};

use super::OnboardingSlide;
use crate::OnboardingEvent;
use crate::model::OnboardingStateModel;

#[derive(Clone, Debug)]
pub enum IntroSlideEvent {
    LoginRequested,
    /// Emitted when the local-backend (warpi) welcome slide's "Open provider
    /// settings" button is clicked. The host opens Settings -> Agents -> Local Pi
    /// provider so the user can add an API key.
    ProviderSettingsRequested,
}

#[derive(Clone, Debug)]
pub enum IntroSlideAction {
    GetStartedClicked,
    LoginClicked,
    ProviderSettingsClicked,
}

/// The warpi wordmark used by the local-backend welcome slide. The same asset as
/// the About page; the onboarding crate cannot import the app's constant.
const WARPI_WORDMARK_PATH: &str = "bundled/png/warpi-wordmark.png";

/// The dark tile color behind the wordmark on light themes. The wordmark is
/// light ink on a transparent canvas, so it needs a dark backdrop to stay legible.
const WARPI_TILE_RGBA: (u8, u8, u8, u8) = (0x0B, 0x0D, 0x10, 0xFF);

pub struct IntroSlide {
    onboarding_state: ModelHandle<OnboardingStateModel>,
    get_started_button: button::Button,
    provider_settings_button: button::Button,
    shimmering_title_handle: ShimmeringTextStateHandle,
    login_mouse_state: MouseStateHandle,
    /// True when the local Pi backend serves inference (standalone/warpi). Selects
    /// the rebranded welcome slide and hides the Warp-account login affordance.
    local_backend: bool,
}

impl IntroSlide {
    pub(crate) fn new(
        onboarding_state: ModelHandle<OnboardingStateModel>,
        local_backend: bool,
    ) -> Self {
        Self {
            onboarding_state,
            get_started_button: button::Button::default(),
            provider_settings_button: button::Button::default(),
            shimmering_title_handle: ShimmeringTextStateHandle::new(),
            login_mouse_state: MouseStateHandle::default(),
            local_backend,
        }
    }
}

impl Entity for IntroSlide {
    type Event = IntroSlideEvent;
}

impl View for IntroSlide {
    fn ui_name() -> &'static str {
        "IntroSlide"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        if self.local_backend {
            return self.render_local_backend(appearance);
        }

        let content = self.render_centered_content(appearance);
        let constrained = ConstrainedBox::new(content).with_max_width(421.).finish();
        // Background is rendered by the parent onboarding view (including background images).
        let centered = Container::new(Align::new(constrained).finish()).finish();

        let sub_text_color = internal_colors::text_sub(theme, theme.background().into_solid());
        let ui_builder = appearance.ui_builder();
        let disclaimer_styles = UiComponentStyles {
            font_color: Some(sub_text_color),
            font_size: Some(12.),
            ..Default::default()
        };

        let login_row = Flex::row()
            .with_child(
                ui_builder
                    .span("Already have an account? ")
                    .with_style(disclaimer_styles)
                    .build()
                    .finish(),
            )
            .with_child(
                ui_builder
                    .link(
                        "Log in".into(),
                        None,
                        Some(Box::new(|ctx| {
                            ctx.dispatch_typed_action(IntroSlideAction::LoginClicked);
                        })),
                        self.login_mouse_state.clone(),
                    )
                    .soft_wrap(false)
                    .with_style(UiComponentStyles {
                        font_size: Some(12.),
                        ..Default::default()
                    })
                    .build()
                    .finish(),
            )
            .finish();

        let mut stack = Stack::new();
        stack.add_child(centered);
        stack.add_positioned_child(
            login_row,
            OffsetPositioning::offset_from_parent(
                vec2f(0., -28.),
                ParentOffsetBounds::ParentBySize,
                ParentAnchor::BottomMiddle,
                ChildAnchor::BottomMiddle,
            ),
        );
        stack.finish()
    }
}

impl IntroSlide {
    fn get_started_clicked(&mut self, ctx: &mut ViewContext<Self>) {
        send_telemetry_from_ctx!(OnboardingEvent::GetStartedClicked, ctx);
        if FeatureFlag::AccountFirstOnboarding.is_enabled() {
            send_telemetry_from_ctx!(
                OnboardingEvent::OnboardingAction {
                    slide_name: "welcome".to_string(),
                    action: "get_started".to_string(),
                    account_class: None,
                },
                ctx
            );
        }

        self.onboarding_state.update(ctx, |model, ctx| {
            model.next(ctx);
        });
    }
}

impl OnboardingSlide for IntroSlide {
    fn on_enter(&mut self, ctx: &mut ViewContext<Self>) {
        self.get_started_clicked(ctx);
    }
}

impl IntroSlide {
    fn render_centered_content(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();

        let logo_fill = internal_colors::fg_overlay_4(theme);
        let logo = ConstrainedBox::new(Icon::WarpLogoLight.to_warpui_icon(logo_fill).finish())
            .with_width(64.)
            .with_height(64.)
            .finish();

        let base_color: ColorU = internal_colors::fg_overlay_4(theme).into();
        let shimmer_color: ColorU = theme.foreground().into();
        let title = ShimmeringTextElement::new(
            "Welcome to Warp",
            appearance.ui_font_family(),
            32.,
            base_color,
            shimmer_color,
            ShimmerConfig::default(),
            self.shimmering_title_handle.clone(),
        )
        .finish();

        let subtitle_color = internal_colors::text_sub(theme, theme.background().into_solid());
        let subtitle = FormattedTextElement::from_str(
            "A modern terminal with state of the art agents built in.",
            appearance.ui_font_family(),
            16.,
        )
        .with_color(subtitle_color)
        .with_alignment(TextAlignment::Center)
        .with_line_height_ratio(1.0)
        .finish();

        let enter = Keystroke::parse("enter").unwrap_or_default();
        let get_started_button = self.get_started_button.render(
            appearance,
            button::Params {
                content: button::Content::Label("Get started".into()),
                theme: &button::themes::Primary,
                options: button::Options {
                    keystroke: Some(enter),
                    on_click: Some(Box::new(|ctx, _app, _pos| {
                        ctx.dispatch_typed_action(IntroSlideAction::GetStartedClicked);
                    })),
                    ..button::Options::default(appearance)
                },
            },
        );

        Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(logo)
            .with_child(title)
            .with_child(Container::new(subtitle).with_margin_top(12.).finish())
            .with_child(
                Container::new(get_started_button)
                    .with_margin_top(24.)
                    .finish(),
            )
            .finish()
    }

    /// The warpi welcome slide. Replaces the Warp-account login affordance with
    /// the concrete steps for adding a local provider API key.
    fn render_local_backend(&self, appearance: &Appearance) -> Box<dyn Element> {
        let content = self.render_local_backend_content(appearance);
        let constrained = ConstrainedBox::new(content).with_max_width(560.).finish();
        // Background is rendered by the parent onboarding view (including background images).
        Container::new(Align::new(constrained).finish()).finish()
    }

    fn render_local_backend_content(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();

        let wordmark = ConstrainedBox::new(
            Image::new(
                AssetSource::Bundled {
                    path: WARPI_WORDMARK_PATH,
                },
                CacheOption::BySize,
            )
            .finish(),
        )
        .with_max_height(56.)
        .with_max_width(240.)
        .finish();

        // The wordmark is light ink on a transparent canvas: it reads on the dark
        // theme but needs a dark tile to stay legible on a light theme.
        let logo: Box<dyn Element> = if theme.inferred_color_scheme() == ColorScheme::LightOnDark {
            wordmark
        } else {
            Container::new(wordmark)
                .with_background_color(ColorU::new(
                    WARPI_TILE_RGBA.0,
                    WARPI_TILE_RGBA.1,
                    WARPI_TILE_RGBA.2,
                    WARPI_TILE_RGBA.3,
                ))
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(10.)))
                .with_horizontal_padding(16.)
                .with_vertical_padding(10.)
                .finish()
        };

        let base_color: ColorU = internal_colors::fg_overlay_4(theme).into();
        let shimmer_color: ColorU = theme.foreground().into();
        let title = ShimmeringTextElement::new(
            "Welcome to warpi",
            appearance.ui_font_family(),
            32.,
            base_color,
            shimmer_color,
            ShimmerConfig::default(),
            self.shimmering_title_handle.clone(),
        )
        .finish();

        let background = theme.background().into_solid();
        let subtitle = FormattedTextElement::from_str(
            "A local agent backend for Warp's terminal. No Warp account required.",
            appearance.ui_font_family(),
            16.,
        )
        .with_color(internal_colors::text_sub(theme, background))
        .with_alignment(TextAlignment::Center)
        .with_line_height_ratio(1.2)
        .finish();

        let steps = self.render_setup_steps(appearance);

        let enter = Keystroke::parse("enter").unwrap_or_default();
        let get_started_button = self.get_started_button.render(
            appearance,
            button::Params {
                content: button::Content::Label("Get started".into()),
                theme: &button::themes::Naked,
                options: button::Options {
                    keystroke: Some(enter),
                    on_click: Some(Box::new(|ctx, _app, _pos| {
                        ctx.dispatch_typed_action(IntroSlideAction::GetStartedClicked);
                    })),
                    ..button::Options::default(appearance)
                },
            },
        );

        let provider_settings_button = self.provider_settings_button.render(
            appearance,
            button::Params {
                content: button::Content::Label("Open provider settings".into()),
                theme: &button::themes::Primary,
                options: button::Options {
                    on_click: Some(Box::new(|ctx, _app, _pos| {
                        ctx.dispatch_typed_action(IntroSlideAction::ProviderSettingsClicked);
                    })),
                    ..button::Options::default(appearance)
                },
            },
        );

        let buttons = Flex::row()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(provider_settings_button)
            .with_child(
                Container::new(get_started_button)
                    .with_margin_left(12.)
                    .finish(),
            )
            .finish();

        Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(logo)
            .with_child(Container::new(title).with_margin_top(20.).finish())
            .with_child(Container::new(subtitle).with_margin_top(12.).finish())
            .with_child(Container::new(steps).with_margin_top(28.).finish())
            .with_child(Container::new(buttons).with_margin_top(28.).finish())
            .finish()
    }

    /// The concrete "add an API key" steps, kept to short skimmable lines.
    fn render_setup_steps(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let background = theme.background().into_solid();
        let main_color = internal_colors::text_main(theme, background);

        let heading = appearance
            .ui_builder()
            .paragraph("Add an API key so the agent works")
            .with_style(UiComponentStyles {
                font_size: Some(15.),
                font_weight: Some(Weight::Medium),
                ..Default::default()
            })
            .build()
            .finish();

        let step = |number: usize, text: &str| {
            Container::new(
                FormattedTextElement::from_str(
                    format!("{number}.  {text}"),
                    appearance.ui_font_family(),
                    14.,
                )
                .with_color(main_color)
                .with_line_height_ratio(1.35)
                .finish(),
            )
            .with_margin_top(10.)
            .finish()
        };

        let note = FormattedTextElement::from_str(
            "The key is stored in your OS secret store (encrypted file fallback). \
             It never goes to a Warp server.",
            appearance.ui_font_family(),
            12.,
        )
        .with_color(internal_colors::text_sub(theme, background))
        .with_line_height_ratio(1.35)
        .finish();

        Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .with_child(heading)
            .with_child(step(1, "Open Settings → Agents → Local Pi provider"))
            .with_child(step(
                2,
                "Pick a preset — DeepSeek, Kimi (Moonshot), or a custom endpoint",
            ))
            .with_child(step(3, "Paste your API key, then click Test connection"))
            .with_child(Container::new(note).with_margin_top(12.).finish())
            .finish()
    }
}

impl TypedActionView for IntroSlide {
    type Action = IntroSlideAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            IntroSlideAction::GetStartedClicked => {
                self.get_started_clicked(ctx);
            }
            IntroSlideAction::LoginClicked => {
                send_telemetry_from_ctx!(OnboardingEvent::WelcomeLoginClicked, ctx);
                ctx.emit(IntroSlideEvent::LoginRequested);
            }
            IntroSlideAction::ProviderSettingsClicked => {
                ctx.emit(IntroSlideEvent::ProviderSettingsRequested);
            }
        }
    }
}
