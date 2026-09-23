use warpui::assets::asset_cache::AssetSource;
use warpui::color::ColorU;
use warpui::elements::{
    Align, CacheOption, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Element, Flex,
    Image, MainAxisAlignment, MouseStateHandle, ParentElement, Radius, Wrap,
};
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, TypedActionView, View, ViewContext, ViewHandle};

use super::SettingsSection;
use super::settings_page::{
    MatchData, PageType, SettingsPageEvent, SettingsPageMeta, SettingsPageViewHandle,
    SettingsWidget,
};
use crate::appearance::Appearance;
use crate::channel::ChannelState;
use crate::standalone_ui::WARPI_VERSION;
use crate::themes::theme::ColorScheme;
use crate::workspace::WorkspaceAction;

pub struct AboutPageView {
    page: PageType<Self>,
}

impl AboutPageView {
    pub fn new(_ctx: &mut ViewContext<AboutPageView>) -> Self {
        AboutPageView {
            page: PageType::new_monolith(AboutPageWidget::default(), None, false),
        }
    }
}

impl Entity for AboutPageView {
    type Event = SettingsPageEvent;
}
impl TypedActionView for AboutPageView {
    type Action = ();
}

impl View for AboutPageView {
    fn ui_name() -> &'static str {
        "AboutPage"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.page.render(self, app)
    }
}

const WARPI_WORDMARK_PATH: &str = "bundled/png/warpi-wordmark.png";
const WARPI_REPO_URL: &str = "https://github.com/mojomast/warpi";

#[derive(Default)]
struct AboutPageWidget {
    copy_version_button_mouse_state: MouseStateHandle,
    repo_link_mouse_state: MouseStateHandle,
}

impl SettingsWidget for AboutPageWidget {
    type View = AboutPageView;

    fn search_terms(&self) -> &str {
        "about warp version"
    }

    fn render(
        &self,
        _view: &AboutPageView,
        appearance: &Appearance,
        _app: &AppContext,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let ui_builder = appearance.ui_builder();

        let wordmark = ConstrainedBox::new(
            Image::new(
                AssetSource::Bundled {
                    path: WARPI_WORDMARK_PATH,
                },
                CacheOption::BySize,
            )
            .finish(),
        )
        .with_max_height(64.)
        .with_max_width(280.)
        .finish();

        // The wordmark is light ink on a transparent canvas: it reads directly on the dark
        // theme, but needs a dark tile to stay legible when the theme is light.
        let logo: Box<dyn Element> = if theme.inferred_color_scheme() == ColorScheme::LightOnDark {
            wordmark
        } else {
            Container::new(wordmark)
                .with_background_color(ColorU::new(0x0B, 0x0D, 0x10, 0xFF))
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
                .with_horizontal_padding(20.)
                .with_vertical_padding(12.)
                .finish()
        };

        let version = ChannelState::app_version().unwrap_or(WARPI_VERSION);

        let version_text = ui_builder
            .span(version.to_string())
            .with_soft_wrap()
            .build()
            .with_margin_top(16.)
            .finish();

        let copy_version_icon = appearance
            .ui_builder()
            .copy_button(16., self.copy_version_button_mouse_state.clone())
            .build()
            .on_click(move |ctx, _, _| {
                ctx.dispatch_typed_action(WorkspaceAction::CopyVersion(version));
            })
            .finish();

        let version_row = Wrap::row()
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_children([
                version_text,
                Container::new(copy_version_icon)
                    .with_margin_top(16.)
                    .with_padding_left(6.)
                    .finish(),
            ]);

        let repo_link = ui_builder
            .link(
                "github.com/mojomast/warpi".to_owned(),
                Some(WARPI_REPO_URL.to_owned()),
                None,
                self.repo_link_mouse_state.clone(),
            )
            .soft_wrap(false)
            .build()
            .with_margin_top(16.)
            .finish();

        Align::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(logo)
                .with_child(version_row.finish())
                .with_child(repo_link)
                .with_child(
                    ui_builder
                        .span("© 2026 warpi contributors. Warp is © 2020–2026 Denver Technologies, Inc.")
                        .with_soft_wrap()
                        .build()
                        .with_margin_top(16.)
                        .finish(),
                )
                .finish(),
        )
        .finish()
    }
}

impl SettingsPageMeta for AboutPageView {
    fn section() -> SettingsSection {
        SettingsSection::About
    }

    fn should_render(&self, _ctx: &AppContext) -> bool {
        true
    }

    fn update_filter(&mut self, query: &str, ctx: &mut ViewContext<Self>) -> MatchData {
        self.page.update_filter(query, ctx)
    }

    fn scroll_to_widget(&mut self, widget_id: &'static str) {
        self.page.scroll_to_widget(widget_id)
    }

    fn clear_highlighted_widget(&mut self) {
        self.page.clear_highlighted_widget();
    }
}

impl From<ViewHandle<AboutPageView>> for SettingsPageViewHandle {
    fn from(view_handle: ViewHandle<AboutPageView>) -> Self {
        SettingsPageViewHandle::About(view_handle)
    }
}
