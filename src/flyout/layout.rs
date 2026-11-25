use crate::audio::model::{
    AppState, DeviceFlow, LISTEN_TARGET_COMMS_OUTPUT, LISTEN_TARGET_DEFAULT_OUTPUT,
};
use winaudio::Win32::Graphics::Gdi::HDC;

use crate::ui::paint::{
    FontCache, FontRole, RectPx, measure_text_width, measure_wrapped_height, px,
};

pub const FLYOUT_WIDTH_LOGICAL: f32 = 468.0;
pub const FLYOUT_HEIGHT_LOGICAL: f32 = 600.0;
pub const EDGE_MARGIN_LOGICAL: f32 = 10.0;
pub const TACTILE_PAD_LEFT_LOGICAL: f32 = 26.0;
pub const TACTILE_PAD_RIGHT_LOGICAL: f32 = 6.0;

const OUTER_PAD_X: f32 = 13.0;
const CONFIG_POPOVER_WIDTH: f32 = 288.0;
const LISTEN_POPOVER_WIDTH: f32 = 396.0;
const LISTEN_POPOVER_MAX_HEIGHT: f32 = 340.0;
const LISTEN_POPOVER_MARGIN: f32 = 12.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveOverlay {
    None,
    Config,
    ProfileMenu,
    ListenTarget(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveInput {
    None,
    Search,
    SaveProfile,
    ListenSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Arrow,
    Hand,
    IBeam,
    ResizeEW,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitAction {
    ToggleLock,
    ToggleConfig,
    TogglePin,
    ToggleProfileMenu,
    ToggleSaveProfileBar,
    FocusSaveProfileInput,
    CommitSaveProfile,
    CancelSaveProfile,
    SetTab(DeviceFlow),
    FocusSearch,
    ClearSearch,
    EmptyClearSearch,
    SetDefault {
        flow: DeviceFlow,
        device_id: String,
    },
    SetComms {
        flow: DeviceFlow,
        device_id: String,
    },
    VolumeBar(String),
    ToggleMute(String),
    ToggleEnhancements(String),
    ToggleDeviceDisabled(String),
    ToggleDevicePin(String),
    ToggleListenEnabled(String),
    ToggleListenMenu(String),
    DismissOverlay,
    OverlaySurface,
    ConfigSeparateComms,
    ConfigAutoDisableEnhancements,
    ConfigReenableEnhancements,
    ConfigAutoDisableUnused,
    ConfigReenableDevices,
    ConfigRestartAudio,
    ConfigQuit,
    HeaderQuit,
    ApplyProfile(String),
    DeleteProfile(String),
    FocusListenSearch,
    ClearListenSearch,
    SetListenTarget {
        device_id: String,
        target_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct HitItem {
    pub rect: RectPx,
    pub action: HitAction,
    pub cursor: CursorKind,
}

pub struct LayoutInput<'a> {
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub state: &'a AppState,
    pub active_tab: DeviceFlow,
    pub search_query: &'a str,
    pub listen_search_query: &'a str,
    pub active_overlay: &'a ActiveOverlay,
    pub saving_profile: bool,
    pub confirm_quit: bool,
    pub device_scroll: i32,
    pub config_scroll: i32,
    pub profile_scroll: i32,
    pub listen_scroll: i32,
}

pub struct HeaderLayout {
    pub title_rect: RectPx,
    pub lock_pill: RectPx,
    pub cluster_rect: RectPx,
    pub config_btn: RectPx,
    pub pin_btn: RectPx,
    pub quit_btn: RectPx,
}

pub struct SaveProfileBarLayout {
    pub input_rect: RectPx,
    pub confirm_btn: RectPx,
    pub cancel_btn: RectPx,
}

pub struct ProfileBarLayout {
    pub selector_btn: RectPx,
    pub selector_label: String,
    pub has_profiles: bool,
    pub save_toggle_btn: RectPx,
    pub save_bar: Option<SaveProfileBarLayout>,
}

pub struct TabsSearchLayout {
    pub tabs_container: RectPx,
    pub output_tab: RectPx,
    pub output_count: usize,
    pub input_tab: RectPx,
    pub input_count: usize,
    pub search_box: RectPx,
    pub search_clear_btn: Option<RectPx>,
}

pub struct ListenRowLayout {
    pub divider_y: i32,
    pub checkbox_hit: RectPx,
    pub checkbox_box: RectPx,
    pub label_rect: RectPx,
    pub target_btn: RectPx,
    pub target_label: String,
    pub has_target: bool,
    pub enabled: bool,
    pub hint_rect: Option<RectPx>,
}

pub struct DeviceCardLayout {
    pub device_id: String,
    pub flow: DeviceFlow,
    pub name: String,
    pub subtitle: String,
    pub disabled: bool,
    pub muted: bool,
    pub volume: u8,
    pub pinned: bool,
    pub is_default: bool,
    pub is_comms: bool,
    pub enh_disabled: bool,
    pub card_rect: RectPx,
    pub name_rect: RectPx,
    pub subtitle_rect: RectPx,
    pub default_pill: RectPx,
    pub comms_pill: Option<RectPx>,
    pub volume_bar: RectPx,
    pub mute_btn: RectPx,
    pub enh_box: RectPx,
    pub enh_label: RectPx,
    pub disable_box: Option<RectPx>,
    pub disable_label: Option<RectPx>,
    pub pin_btn: RectPx,
    pub listen: Option<ListenRowLayout>,
}

pub struct EmptyStateLayout {
    pub card_rect: RectPx,
    pub title_rect: RectPx,
    pub subtitle: Option<(RectPx, String)>,
    pub clear_btn: Option<RectPx>,
}

pub struct FooterLayout {
    pub top_y: i32,
    pub dot_rect: RectPx,
    pub status_rect: RectPx,
}

pub enum ConfigRowKind {
    Header {
        version: String,
    },
    CheckboxItem {
        action: HitAction,
        checked: bool,
        title: &'static str,
        subtitle: Option<&'static str>,
        checkbox_box: RectPx,
        title_rect: RectPx,
        subtitle_rect: Option<RectPx>,
    },
    ActionButton {
        action: HitAction,
        label: String,
        inverted: bool,
        btn_rect: RectPx,
    },
    Divider {
        y: i32,
        left: i32,
        right: i32,
    },
    QuitRow {
        icon_rect: RectPx,
        label_rect: RectPx,
        confirm_quit: bool,
    },
}

pub struct ConfigRowLayout {
    pub rect: RectPx,
    pub kind: ConfigRowKind,
}

pub struct ConfigOverlayLayout {
    pub panel_rect: RectPx,
    pub viewport_rect: RectPx,
    pub rows: Vec<ConfigRowLayout>,
    pub max_scroll: i32,
    pub scroll: i32,
    pub content_height: i32,
}

pub struct ProfileOptionLayout {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub row_rect: RectPx,
    pub check_rect: RectPx,
    pub label_rect: RectPx,
    pub delete_btn: RectPx,
}

pub struct ProfileOverlayLayout {
    pub panel_rect: RectPx,
    pub viewport_rect: RectPx,
    pub rows: Vec<ProfileOptionLayout>,
    pub empty_rect: Option<RectPx>,
    pub max_scroll: i32,
    pub scroll: i32,
    pub content_height: i32,
}

pub struct ListenDefaultEntryLayout {
    pub target_id: &'static str,
    pub title: &'static str,
    pub subtext: Option<String>,
    pub active: bool,
    pub rect: RectPx,
    pub check_rect: RectPx,
    pub title_rect: RectPx,
    pub subtext_rect: Option<RectPx>,
    pub radii: [f32; 4],
}

pub struct ListenDeviceOptionLayout {
    pub target_id: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub disabled: bool,
    pub active: bool,
    pub rect: RectPx,
    pub check_rect: RectPx,
    pub title_rect: RectPx,
    pub disabled_pill: Option<RectPx>,
    pub subtitle_rect: Option<RectPx>,
}

pub struct ListenOverlayLayout {
    pub input_device_id: String,
    pub panel_rect: RectPx,
    pub search_box: RectPx,
    pub search_clear_btn: Option<RectPx>,
    pub viewport_rect: RectPx,
    pub default_section_rect: Option<RectPx>,
    pub default_divider_y: Option<i32>,
    pub default_entries: Vec<ListenDefaultEntryLayout>,
    pub section_divider_y: Option<i32>,
    pub device_entries: Vec<ListenDeviceOptionLayout>,
    pub empty_rect: Option<RectPx>,
    pub max_scroll: i32,
    pub scroll: i32,
    pub content_height: i32,
}

pub enum OverlayLayout {
    None,
    Config(ConfigOverlayLayout),
    Profile(ProfileOverlayLayout),
    Listen(ListenOverlayLayout),
}

pub struct FlyoutLayout {
    pub width: i32,
    pub height: i32,
    pub header: HeaderLayout,
    pub profile_bar: ProfileBarLayout,
    pub tabs_search: TabsSearchLayout,
    pub list_viewport: RectPx,
    pub cards: Vec<DeviceCardLayout>,
    pub empty_state: Option<EmptyStateLayout>,
    pub list_max_scroll: i32,
    pub list_scroll: i32,
    pub list_content_height: i32,
    pub footer: FooterLayout,
    pub overlay: OverlayLayout,
    pub hit_items: Vec<HitItem>,
}

impl FlyoutLayout {
    pub fn compute(hdc: HDC, fonts: &FontCache, input: &LayoutInput<'_>) -> Self {
        let s = input.scale;
        let width = input.width;
        let height = input.height;
        let pad_x = px(OUTER_PAD_X, s);
        let content_left = pad_x;
        let content_right = width - pad_x;
        let mut hit_items = Vec::with_capacity(64);

        let (header, mut y) = compute_header(hdc, fonts, input, content_left, content_right, &mut hit_items);
        let (profile_bar, next_y) =
            compute_profile_bar(hdc, fonts, input, content_left, content_right, y, &mut hit_items);
        y = next_y;
        let (tabs_search, list_top) =
            compute_tabs_search(hdc, fonts, input, content_left, content_right, y, &mut hit_items);

        let footer = compute_footer(height, s, content_left, content_right);
        let list_viewport = RectPx::new(0, list_top, width, footer.top_y);

        let (cards, empty_state, list_max_scroll, list_scroll, list_content_height) =
            compute_device_list(
                hdc,
                fonts,
                input,
                content_left,
                content_right,
                list_viewport,
                &mut hit_items,
            );

        let overlay = match input.active_overlay {
            ActiveOverlay::None => OverlayLayout::None,
            ActiveOverlay::Config => OverlayLayout::Config(compute_config_overlay(
                hdc,
                fonts,
                input,
                header.cluster_rect,
                &mut hit_items,
            )),
            ActiveOverlay::ProfileMenu => OverlayLayout::Profile(compute_profile_overlay(
                input,
                profile_bar.selector_btn,
                &mut hit_items,
            )),
            ActiveOverlay::ListenTarget(dev_id) => {
                let anchor = cards
                    .iter()
                    .find(|c| c.device_id == *dev_id)
                    .and_then(|c| c.listen.as_ref())
                    .map(|l| l.target_btn);
                OverlayLayout::Listen(compute_listen_overlay(
                    hdc,
                    fonts,
                    input,
                    dev_id,
                    anchor,
                    &mut hit_items,
                ))
            }
        };

        Self {
            width,
            height,
            header,
            profile_bar,
            tabs_search,
            list_viewport,
            cards,
            empty_state,
            list_max_scroll,
            list_scroll,
            list_content_height,
            footer,
            overlay,
            hit_items,
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<&HitItem> {
        self.hit_items.iter().rev().find(|item| item.rect.contains(x, y))
    }

    pub fn hovered_card_id(&self, x: i32, y: i32) -> Option<&str> {
        if !matches!(self.overlay, OverlayLayout::None) || !self.list_viewport.contains(x, y) {
            return None;
        }
        self.cards
            .iter()
            .find(|c| c.card_rect.contains(x, y))
            .map(|c| c.device_id.as_str())
    }

    pub fn slider_rect(&self, device_id: &str) -> Option<RectPx> {
        self.cards
            .iter()
            .find(|c| c.device_id == device_id && !c.disabled)
            .map(|c| c.volume_bar)
    }

    pub fn card(&self, device_id: &str) -> Option<&DeviceCardLayout> {
        self.cards.iter().find(|c| c.device_id == device_id)
    }

    pub fn update_card_volume(
        &mut self,
        device_id: &str,
        volume: u8,
        muted: bool,
    ) -> Option<RectPx> {
        let card = self
            .cards
            .iter_mut()
            .find(|c| c.device_id == device_id && !c.disabled)?;
        card.volume = volume.min(100);
        card.muted = muted;
        Some(card.card_rect.intersect(self.list_viewport))
    }

    pub fn overlay_max_scroll(&self) -> i32 {
        match &self.overlay {
            OverlayLayout::None => 0,
            OverlayLayout::Config(c) => c.max_scroll,
            OverlayLayout::Profile(p) => p.max_scroll,
            OverlayLayout::Listen(l) => l.max_scroll,
        }
    }
}

fn compute_header(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    left: i32,
    right: i32,
    hits: &mut Vec<HitItem>,
) -> (HeaderLayout, i32) {
    let s = input.scale;
    let pt = px(9.0, s);
    let pb = px(7.0, s);
    let btn_w = px(28.0, s);
    let btn_h = px(24.0, s);
    let bw = px(1.0, s).max(1);
    let cluster_h = btn_h + bw * 2;
    let cluster_w = btn_w * 3 + bw * 4;
    let row_h = cluster_h;
    let top = pt;

    let cluster_rect = RectPx::from_xywh(right - cluster_w, top, cluster_w, cluster_h);
    let inner_top = cluster_rect.top + bw;
    let config_btn = RectPx::from_xywh(cluster_rect.left + bw, inner_top, btn_w, btn_h);
    let pin_btn = RectPx::from_xywh(config_btn.right + bw, inner_top, btn_w, btn_h);
    let quit_btn = RectPx::from_xywh(pin_btn.right + bw, inner_top, btn_w, btn_h);

    let title_w = measure_text_width(hdc, fonts.get(FontRole::Title14Semi), "SETTMetter Audio");
    let title_rect = RectPx::new(left, top, left + title_w + px(2.0, s), top + row_h);

    let lock_label = if input.state.lock_enabled {
        "Locked"
    } else {
        "Unlocked"
    };
    let lock_text_w = measure_text_width(hdc, fonts.get(FontRole::Body11Semi), lock_label);
    let pill_h = px(22.0, s);
    let pill_w = px(6.0 + 12.0 + 4.0 + 8.0, s) + lock_text_w;
    let pill_left = title_rect.right + px(6.0, s);
    let pill_top = top + (row_h - pill_h) / 2;
    let lock_pill = RectPx::from_xywh(pill_left, pill_top, pill_w, pill_h);

    hits.push(HitItem {
        rect: lock_pill,
        action: HitAction::ToggleLock,
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: config_btn,
        action: HitAction::ToggleConfig,
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: pin_btn,
        action: HitAction::TogglePin,
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: quit_btn,
        action: HitAction::HeaderQuit,
        cursor: CursorKind::Hand,
    });

    (
        HeaderLayout {
            title_rect,
            lock_pill,
            cluster_rect,
            config_btn,
            pin_btn,
            quit_btn,
        },
        top + row_h + pb,
    )
}

fn compute_profile_bar(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    left: i32,
    right: i32,
    top: i32,
    hits: &mut Vec<HitItem>,
) -> (ProfileBarLayout, i32) {
    let s = input.scale;
    let has_profiles = !input.state.profiles.is_empty();
    let selector_label = input
        .state
        .active_profile()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| {
            if has_profiles {
                "Custom".to_string()
            } else {
                "No profiles".to_string()
            }
        });

    let row_h = px(26.0, s);
    let gap = px(6.0, s);
    let text_w = measure_text_width(hdc, fonts.get(FontRole::Body11_5Med), &selector_label);
    let raw_sel_w = px(9.0 + 7.0 + 8.0 + 11.0, s) + text_w;
    let sel_w = raw_sel_w.clamp(px(126.0, s), px(220.0, s));
    let selector_btn = RectPx::from_xywh(left, top, sel_w, row_h);

    let save_text_w = measure_text_width(hdc, fonts.get(FontRole::Body11_5Med), "Save");
    let save_w = px(9.0 + 11.0 + 4.0 + 9.0, s) + save_text_w;
    let save_toggle_btn = RectPx::from_xywh(selector_btn.right + gap, top, save_w, row_h);

    hits.push(HitItem {
        rect: selector_btn,
        action: HitAction::ToggleProfileMenu,
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: save_toggle_btn,
        action: HitAction::ToggleSaveProfileBar,
        cursor: CursorKind::Hand,
    });

    let mut y = top + row_h;
    let save_bar = if input.saving_profile {
        let bar_top = y + gap;
        let cancel_w = row_h;
        let confirm_text_w = measure_text_width(hdc, fonts.get(FontRole::Body11Semi), "Save");
        let confirm_w = px(20.0, s) + confirm_text_w;
        let cancel_btn = RectPx::from_xywh(right - cancel_w, bar_top, cancel_w, row_h);
        let confirm_btn =
            RectPx::from_xywh(cancel_btn.left - gap - confirm_w, bar_top, confirm_w, row_h);
        let input_rect = RectPx::new(left, bar_top, (confirm_btn.left - gap).max(left), bar_top + row_h);

        hits.push(HitItem {
            rect: input_rect,
            action: HitAction::FocusSaveProfileInput,
            cursor: CursorKind::IBeam,
        });
        hits.push(HitItem {
            rect: confirm_btn,
            action: HitAction::CommitSaveProfile,
            cursor: CursorKind::Hand,
        });
        hits.push(HitItem {
            rect: cancel_btn,
            action: HitAction::CancelSaveProfile,
            cursor: CursorKind::Hand,
        });

        y = bar_top + row_h;
        Some(SaveProfileBarLayout {
            input_rect,
            confirm_btn,
            cancel_btn,
        })
    } else {
        None
    };

    (
        ProfileBarLayout {
            selector_btn,
            selector_label,
            has_profiles,
            save_toggle_btn,
            save_bar,
        },
        y + px(7.0, s),
    )
}

fn compute_tabs_search(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    left: i32,
    right: i32,
    top: i32,
    hits: &mut Vec<HitItem>,
) -> (TabsSearchLayout, i32) {
    let s = input.scale;
    let out_count = input.state.devices_for(DeviceFlow::Output).count();
    let in_count = input.state.devices_for(DeviceFlow::Input).count();

    let pad = px(2.0, s);
    let bw = px(1.0, s).max(1);
    let tab_h = px(24.0, s);
    let tab_gap = px(2.0, s);

    let out_label_w = measure_text_width(hdc, fonts.get(FontRole::Body11_5Med), "Output");
    let out_cnt_w =
        measure_text_width(hdc, fonts.get(FontRole::Tiny10Semi), &out_count.to_string());
    let out_w = px(20.0 + 5.0, s) + out_label_w + out_cnt_w;

    let in_label_w = measure_text_width(hdc, fonts.get(FontRole::Body11_5Med), "Input");
    let in_cnt_w = measure_text_width(hdc, fonts.get(FontRole::Tiny10Semi), &in_count.to_string());
    let in_w = px(20.0 + 5.0, s) + in_label_w + in_cnt_w;

    let container_h = tab_h + (pad + bw) * 2;
    let container_w = out_w + in_w + tab_gap + (pad + bw) * 2;
    let search_h = px(30.0, s);
    let row_h = container_h.max(search_h);

    let container_top = top + (row_h - container_h) / 2;
    let tabs_container = RectPx::from_xywh(left, container_top, container_w, container_h);
    let tab_top = container_top + pad + bw;
    let output_tab = RectPx::from_xywh(left + pad + bw, tab_top, out_w, tab_h);
    let input_tab = RectPx::from_xywh(output_tab.right + tab_gap, tab_top, in_w, tab_h);

    let search_left = tabs_container.right + px(8.0, s);
    let search_top = top + (row_h - search_h) / 2;
    let search_box = RectPx::new(search_left, search_top, right, search_top + search_h);

    let search_clear_btn = (!input.search_query.is_empty()).then(|| {
        let btn_s = px(18.0, s);
        let btn_right = search_box.right - px(5.0, s);
        let btn_top = search_box.top + (search_box.height() - btn_s) / 2;
        RectPx::from_xywh(btn_right - btn_s, btn_top, btn_s, btn_s)
    });

    hits.push(HitItem {
        rect: output_tab,
        action: HitAction::SetTab(DeviceFlow::Output),
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: input_tab,
        action: HitAction::SetTab(DeviceFlow::Input),
        cursor: CursorKind::Hand,
    });
    hits.push(HitItem {
        rect: search_box,
        action: HitAction::FocusSearch,
        cursor: CursorKind::IBeam,
    });
    if let Some(clear_rect) = search_clear_btn {
        hits.push(HitItem {
            rect: clear_rect,
            action: HitAction::ClearSearch,
            cursor: CursorKind::Hand,
        });
    }

    (
        TabsSearchLayout {
            tabs_container,
            output_tab,
            output_count: out_count,
            input_tab,
            input_count: in_count,
            search_box,
            search_clear_btn,
        },
        top + row_h + px(8.0, s),
    )
}

fn compute_footer(
    height: i32,
    s: f32,
    left: i32,
    right: i32,
) -> FooterLayout {
    let pt = px(6.0, s);
    let pb = px(7.0, s);
    let row_h = px(22.0, s);
    let divider_h = px(1.0, s).max(1);
    let footer_h = divider_h + pt + row_h + pb;
    let top_y = height - footer_h;

    let row_top = top_y + divider_h + pt;
    let dot_d = px(5.0, s).max(4);
    let dot_top = row_top + (row_h - dot_d) / 2;
    let dot_rect = RectPx::from_xywh(left, dot_top, dot_d, dot_d);
    let status_rect = RectPx::new(
        dot_rect.right + px(5.0, s),
        row_top,
        right,
        row_top + row_h,
    );

    FooterLayout {
        top_y,
        dot_rect,
        status_rect,
    }
}

fn compute_device_list(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    left: i32,
    right: i32,
    viewport: RectPx,
    hits: &mut Vec<HitItem>,
) -> (Vec<DeviceCardLayout>, Option<EmptyStateLayout>, i32, i32, i32) {
    let s = input.scale;
    let devices = input
        .state
        .filtered_devices(input.active_tab, input.search_query);

    if devices.is_empty() {
        let query = input.search_query.trim();
        let has_query = !query.is_empty();
        let py = px(26.0, s);
        let gap = px(6.0, s);
        let title_h = px(16.0, s);
        let sub_h = px(14.0, s);
        let btn_h = px(22.0, s);
        let mut h = py * 2 + title_h;
        if has_query {
            h += gap + sub_h + gap + px(2.0, s) + btn_h;
        }

        let card_rect = RectPx::from_xywh(left, viewport.top, right - left, h);
        let inner_l = card_rect.left + px(12.0, s);
        let inner_r = card_rect.right - px(12.0, s);
        let mut cy = card_rect.top + py;
        let title_rect = RectPx::new(inner_l, cy, inner_r, cy + title_h);
        cy += title_h;

        let (subtitle, clear_btn) = if has_query {
            cy += gap;
            let tab_word = match input.active_tab {
                DeviceFlow::Output => "output",
                DeviceFlow::Input => "input",
            };
            let sub_text = format!("No {tab_word} devices match \"{query}\"");
            let sub_rect = RectPx::new(inner_l, cy, inner_r, cy + sub_h);
            cy += sub_h + gap + px(2.0, s);

            let btn_tw = measure_text_width(hdc, fonts.get(FontRole::Sub10_5Med), "Clear search");
            let btn_w = btn_tw + px(18.0, s);
            let btn_x = card_rect.left + (card_rect.width() - btn_w) / 2;
            let btn_rect = RectPx::from_xywh(btn_x, cy, btn_w, btn_h);
            let clipped = btn_rect.intersect(viewport);
            if !clipped.is_empty() {
                hits.push(HitItem {
                    rect: clipped,
                    action: HitAction::EmptyClearSearch,
                    cursor: CursorKind::Hand,
                });
            }
            (Some((sub_rect, sub_text)), Some(btn_rect))
        } else {
            (None, None)
        };

        return (
            Vec::new(),
            Some(EmptyStateLayout {
                card_rect,
                title_rect,
                subtitle,
                clear_btn,
            }),
            0,
            0,
            h,
        );
    }

    let card_gap = px(6.0, s);
    let pad_bottom = px(9.0, s);
    let card_px = px(10.0, s);
    let card_py = px(8.0, s);
    let row_gap = px(6.0, s);
    let header_h = px(31.0, s);
    let bar_h = px(26.0, s);
    let footer_h = px(20.0, s);
    let listen_div_h = px(1.0, s).max(1) + px(5.0, s);
    let listen_row_h = px(22.0, s);
    let listen_hint_extra = px(3.0 + 14.0, s);

    let card_heights: Vec<i32> = devices
        .iter()
        .map(|dev| {
            let mut h = card_py * 2 + header_h + row_gap + bar_h + row_gap + footer_h;
            if dev.flow == DeviceFlow::Input {
                h += row_gap + listen_div_h + listen_row_h;
                if dev.listen.as_ref().is_some_and(|l| l.enabled) {
                    h += listen_hint_extra;
                }
            }
            h
        })
        .collect();

    let total_cards_h: i32 = card_heights.iter().sum::<i32>()
        + (card_heights.len().saturating_sub(1) as i32) * card_gap
        + pad_bottom;
    let max_scroll = (total_cards_h - viewport.height()).max(0);
    let scroll = input.device_scroll.clamp(0, max_scroll);

    let def_pill_w =
        measure_text_width(hdc, fonts.get(FontRole::Sub10_5Semi), "Default") + px(15.0, s);
    let com_pill_w =
        measure_text_width(hdc, fonts.get(FontRole::Sub10_5Semi), "Communications") + px(15.0, s);
    let pill_h = px(21.0, s);
    let pill_gap = px(4.0, s);

    let chk_size = px(14.0, s);
    let chk_gap = px(6.0, s);
    let enh_text_w =
        measure_text_width(hdc, fonts.get(FontRole::Body11Med), "Disable enhancements");
    let dis_text_w = measure_text_width(hdc, fonts.get(FontRole::Body11Med), "Disabled");
    let dis_off_w = measure_text_width(hdc, fonts.get(FontRole::Body11Med), "Disable");
    let pinned_text_w = measure_text_width(hdc, fonts.get(FontRole::Sub10_5Med), "Pinned");
    let listen_chk_text_w =
        measure_text_width(hdc, fonts.get(FontRole::Body11Med), "Listen to this device");

    let mut cards = Vec::with_capacity(devices.len());
    let mut y = viewport.top - scroll;

    for (dev, card_h) in devices.into_iter().zip(card_heights) {
        let card_rect = RectPx::from_xywh(left, y, right - left, card_h);
        y += card_h + card_gap;

        let inner_l = card_rect.left + card_px;
        let inner_r = card_rect.right - card_px;
        let mut ry = card_rect.top + card_py;

        let is_default = input.state.is_default(dev);
        let is_comms = input.state.is_comms(dev);

        let pill_y = ry + (header_h - pill_h) / 2;
        let (default_pill, comms_pill, pills_left) = if input.state.separate_comms_role {
            let com = RectPx::from_xywh(inner_r - com_pill_w, pill_y, com_pill_w, pill_h);
            let def = RectPx::from_xywh(com.left - pill_gap - def_pill_w, pill_y, def_pill_w, pill_h);
            (def, Some(com), def.left)
        } else {
            let def = RectPx::from_xywh(inner_r - def_pill_w, pill_y, def_pill_w, pill_h);
            (def, None, def.left)
        };

        let text_r = (pills_left - px(8.0, s)).max(inner_l);
        let name_h = px(16.0, s);
        let sub_h = px(14.0, s);
        let name_rect = RectPx::new(inner_l, ry, text_r, ry + name_h);
        let subtitle_rect = RectPx::new(inner_l, name_rect.bottom + px(1.0, s), text_r, ry + header_h.min(name_h + px(1.0, s) + sub_h));
        ry += header_h + row_gap;

        let volume_bar = RectPx::from_xywh(inner_l, ry, inner_r - inner_l, bar_h);
        let mute_w = px(22.0, s);
        let mute_h = px(20.0, s);
        let mute_btn = RectPx::from_xywh(
            volume_bar.left + px(3.0, s),
            volume_bar.top + (bar_h - mute_h) / 2,
            mute_w,
            mute_h,
        );
        ry += bar_h + row_gap;

        let chk_top = ry + (footer_h - chk_size) / 2;
        let enh_box = RectPx::from_xywh(inner_l, chk_top, chk_size, chk_size);
        let enh_label = RectPx::new(
            enh_box.right + chk_gap,
            ry,
            enh_box.right + chk_gap + enh_text_w + px(2.0, s),
            ry + footer_h,
        );
        let enh_hit = RectPx::new(inner_l, ry, enh_label.right, ry + footer_h);

        let (disable_hit, disable_box, disable_label) = if !input.state.auto_disable_unused_devices {
            let dl = enh_hit.right + px(12.0, s);
            let dbox = RectPx::from_xywh(dl, chk_top, chk_size, chk_size);
            let dw = if dev.disabled { dis_text_w } else { dis_off_w };
            let dlab = RectPx::new(
                dbox.right + chk_gap,
                ry,
                dbox.right + chk_gap + dw + px(2.0, s),
                ry + footer_h,
            );
            let dhit = RectPx::new(dl, ry, dlab.right, ry + footer_h);
            (Some(dhit), Some(dbox), Some(dlab))
        } else {
            (None, None, None)
        };

        let pin_w = if dev.pinned {
            px(5.5 + 11.5 + 4.0 + 7.0, s) + pinned_text_w
        } else {
            px(22.0, s)
        };
        let pin_btn = RectPx::from_xywh(inner_r - pin_w, ry, pin_w, footer_h);
        ry += footer_h;

        let listen = if dev.flow == DeviceFlow::Input {
            ry += row_gap;
            let divider_y = ry;
            ry += listen_div_h;

            let l_cfg = dev.listen.clone().unwrap_or_default();
            let l_chk_top = ry + (listen_row_h - chk_size) / 2;
            let checkbox_box = RectPx::from_xywh(inner_l, l_chk_top, chk_size, chk_size);
            let label_rect = RectPx::new(
                checkbox_box.right + chk_gap,
                ry,
                checkbox_box.right + chk_gap + listen_chk_text_w + px(2.0, s),
                ry + listen_row_h,
            );
            let checkbox_hit = RectPx::new(inner_l, ry, label_rect.right, ry + listen_row_h);

            let target_label = input
                .state
                .listen_target_label(l_cfg.target_device_id.as_deref());
            let target_tw =
                measure_text_width(hdc, fonts.get(FontRole::Sub10_5Med), &target_label);
            let desired_btn_w = px(7.0 + 6.0 + 6.0 + 10.5, s) + target_tw;
            let avail_w = (inner_r - checkbox_hit.right - px(8.0, s)).max(px(80.0, s));
            let btn_w = desired_btn_w
                .clamp(px(110.0, s).min(avail_w), px(276.0, s).min(avail_w));
            let target_btn = RectPx::from_xywh(inner_r - btn_w, ry, btn_w, listen_row_h);

            let hint_rect = l_cfg.enabled.then(|| {
                let hy = ry + listen_row_h + px(3.0, s);
                RectPx::new(inner_l + px(20.0, s), hy, inner_r, hy + px(14.0, s))
            });

            Some(ListenRowLayout {
                divider_y,
                checkbox_hit,
                checkbox_box,
                label_rect,
                target_btn,
                target_label,
                has_target: l_cfg.target_device_id.is_some(),
                enabled: l_cfg.enabled,
                hint_rect,
            })
        } else {
            None
        };

        let visible_card = card_rect.intersect(viewport);
        if !visible_card.is_empty() {
            push_clipped_hit(
                hits,
                default_pill,
                viewport,
                HitAction::SetDefault {
                    flow: dev.flow,
                    device_id: dev.id.clone(),
                },
                CursorKind::Hand,
            );
            if let Some(com) = comms_pill {
                push_clipped_hit(
                    hits,
                    com,
                    viewport,
                    HitAction::SetComms {
                        flow: dev.flow,
                        device_id: dev.id.clone(),
                    },
                    CursorKind::Hand,
                );
            }
            if !dev.disabled {
                push_clipped_hit(
                    hits,
                    volume_bar,
                    viewport,
                    HitAction::VolumeBar(dev.id.clone()),
                    CursorKind::ResizeEW,
                );
                push_clipped_hit(
                    hits,
                    mute_btn,
                    viewport,
                    HitAction::ToggleMute(dev.id.clone()),
                    CursorKind::Hand,
                );
            }
            push_clipped_hit(
                hits,
                enh_hit,
                viewport,
                HitAction::ToggleEnhancements(dev.id.clone()),
                CursorKind::Hand,
            );
            if let Some(dhit) = disable_hit {
                push_clipped_hit(
                    hits,
                    dhit,
                    viewport,
                    HitAction::ToggleDeviceDisabled(dev.id.clone()),
                    CursorKind::Hand,
                );
            }
            push_clipped_hit(
                hits,
                pin_btn,
                viewport,
                HitAction::ToggleDevicePin(dev.id.clone()),
                CursorKind::Hand,
            );
            if let Some(ref lrow) = listen {
                push_clipped_hit(
                    hits,
                    lrow.checkbox_hit,
                    viewport,
                    HitAction::ToggleListenEnabled(dev.id.clone()),
                    CursorKind::Hand,
                );
                push_clipped_hit(
                    hits,
                    lrow.target_btn,
                    viewport,
                    HitAction::ToggleListenMenu(dev.id.clone()),
                    CursorKind::Hand,
                );
            }
        }

        cards.push(DeviceCardLayout {
            device_id: dev.id.clone(),
            flow: dev.flow,
            name: dev.name.clone(),
            subtitle: dev.subtitle.clone(),
            disabled: dev.disabled,
            muted: dev.muted,
            volume: dev.volume.min(100),
            pinned: dev.pinned,
            is_default,
            is_comms,
            enh_disabled: input.state.are_enhancements_disabled(dev),
            card_rect,
            name_rect,
            subtitle_rect,
            default_pill,
            comms_pill,
            volume_bar,
            mute_btn,
            enh_box,
            enh_label,
            disable_box,
            disable_label,
            pin_btn,
            listen,
        });
    }

    (cards, None, max_scroll, scroll, total_cards_h)
}

fn compute_config_overlay(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    cluster_rect: RectPx,
    hits: &mut Vec<HitItem>,
) -> ConfigOverlayLayout {
    let s = input.scale;
    let margin = px(8.0, s);
    let pop_w = px(CONFIG_POPOVER_WIDTH, s).min(input.width - margin * 2);
    let pop_x = (cluster_rect.right - pop_w).clamp(margin, (input.width - margin - pop_w).max(margin));
    let pop_y = (cluster_rect.bottom + px(4.0, s)).clamp(margin, (input.height - px(160.0, s)).max(margin));
    let max_h = (input.height - pop_y - margin).max(px(160.0, s));

    let pad = px(6.0, s);
    let gap = px(2.0, s);
    let inner_l = pop_x + pad;
    let inner_r = pop_x + pop_w - pad;
    let inner_w = inner_r - inner_l;

    let chk_size = px(14.0, s);
    let row_px = px(6.0, s);
    let row_py = px(6.0, s);
    let chk_gap = px(8.0, s);
    let text_w = (inner_w - row_px * 2 - chk_size - chk_gap).max(40);
    let title_h = px(15.0, s);
    let desc_gap = px(2.0, s);

    enum Spec {
        Header,
        Check {
            action: HitAction,
            checked: bool,
            title: &'static str,
            subtitle: Option<&'static str>,
            height: i32,
            sub_h: i32,
        },
        Button {
            action: HitAction,
            label: String,
            inverted: bool,
            height: i32,
        },
        Divider {
            height: i32,
        },
        Quit {
            height: i32,
        },
    }

    let make_check = |action: HitAction,
                      checked: bool,
                      title: &'static str,
                      subtitle: Option<&'static str>|
     -> Spec {
        let sub_h = subtitle
            .map(|sub| measure_wrapped_height(hdc, fonts.get(FontRole::Sub10_5Reg), sub, text_w))
            .unwrap_or(0);
        let body_h = if sub_h > 0 {
            title_h + desc_gap + sub_h
        } else {
            title_h.max(chk_size)
        };
        Spec::Check {
            action,
            checked,
            title,
            subtitle,
            height: row_py * 2 + body_h,
            sub_h,
        }
    };

    let mut specs = Vec::new();
    specs.push(Spec::Header);
    specs.push(make_check(
        HitAction::ConfigSeparateComms,
        input.state.separate_comms_role,
        "Separate Default & Communications",
        Some("Show separate Default & Comms buttons"),
    ));
    specs.push(make_check(
        HitAction::ConfigAutoDisableEnhancements,
        input.state.auto_disable_enhancements,
        "Automatically disable enhancements",
        Some("Disable enhancements across devices automatically"),
    ));
    if !input.state.auto_disable_enhancements {
        specs.push(Spec::Button {
            action: HitAction::ConfigReenableEnhancements,
            label: "Re-enable enhancements".to_string(),
            inverted: false,
            height: px(26.0 + 4.0, s),
        });
    }
    specs.push(make_check(
        HitAction::ConfigAutoDisableUnused,
        input.state.auto_disable_unused_devices,
        "Automatically disable unused devices",
        Some("Keep Default, Comms & pinned devices enabled"),
    ));
    if !input.state.auto_disable_unused_devices {
        specs.push(Spec::Button {
            action: HitAction::ConfigReenableDevices,
            label: "Re-enable all devices".to_string(),
            inverted: false,
            height: px(26.0 + 4.0, s),
        });
    }
    specs.push(Spec::Button {
        action: HitAction::ConfigRestartAudio,
        label: "Restart audio service".to_string(),
        inverted: false,
        height: px(26.0 + 4.0, s),
    });
    specs.push(Spec::Divider {
        height: px(9.0, s),
    });
    specs.push(Spec::Quit {
        height: px(28.0, s),
    });

    let header_h = px(19.0, s);
    let content_height: i32 = specs
        .iter()
        .map(|sp| match sp {
            Spec::Header => header_h,
            Spec::Check { height, .. }
            | Spec::Button { height, .. }
            | Spec::Divider { height }
            | Spec::Quit { height } => *height,
        })
        .sum::<i32>()
        + (specs.len().saturating_sub(1) as i32) * gap;

    let panel_h = (content_height + pad * 2).min(max_h);
    let panel_rect = RectPx::from_xywh(pop_x, pop_y, pop_w, panel_h);
    let viewport_rect = RectPx::new(
        panel_rect.left + pad,
        panel_rect.top + pad,
        panel_rect.right - pad,
        panel_rect.bottom - pad,
    );
    let max_scroll = (content_height - viewport_rect.height()).max(0);
    let scroll = input.config_scroll.clamp(0, max_scroll);

    hits.push(HitItem {
        rect: RectPx::new(0, 0, input.width, input.height),
        action: HitAction::DismissOverlay,
        cursor: CursorKind::Arrow,
    });
    hits.push(HitItem {
        rect: panel_rect,
        action: HitAction::OverlaySurface,
        cursor: CursorKind::Arrow,
    });

    let mut rows = Vec::with_capacity(specs.len());
    let mut y = viewport_rect.top - scroll;

    for spec in specs {
        match spec {
            Spec::Header => {
                let rect = RectPx::from_xywh(inner_l, y, inner_w, header_h);
                rows.push(ConfigRowLayout {
                    rect,
                    kind: ConfigRowKind::Header {
                        version: format!("v{}", env!("CARGO_PKG_VERSION")),
                    },
                });
                y += header_h + gap;
            }
            Spec::Check {
                action,
                checked,
                title,
                subtitle,
                height,
                sub_h,
            } => {
                let rect = RectPx::from_xywh(inner_l, y, inner_w, height);
                let box_top = if subtitle.is_some() {
                    rect.top + row_py + px(1.5, s)
                } else {
                    rect.top + (height - chk_size) / 2
                };
                let checkbox_box = RectPx::from_xywh(rect.left + row_px, box_top, chk_size, chk_size);
                let text_l = checkbox_box.right + chk_gap;
                let text_r = rect.right - row_px;
                let (title_rect, subtitle_rect) = if subtitle.is_some() {
                    let tr = RectPx::new(text_l, rect.top + row_py, text_r, rect.top + row_py + title_h);
                    let sr = RectPx::new(text_l, tr.bottom + desc_gap, text_r, tr.bottom + desc_gap + sub_h);
                    (tr, Some(sr))
                } else {
                    (RectPx::new(text_l, rect.top, text_r, rect.bottom), None)
                };
                push_clipped_hit(hits, rect, viewport_rect, action.clone(), CursorKind::Hand);
                rows.push(ConfigRowLayout {
                    rect,
                    kind: ConfigRowKind::CheckboxItem {
                        action,
                        checked,
                        title,
                        subtitle,
                        checkbox_box,
                        title_rect,
                        subtitle_rect,
                    },
                });
                y += height + gap;
            }
            Spec::Button {
                action,
                label,
                inverted,
                height,
            } => {
                let rect = RectPx::from_xywh(inner_l, y, inner_w, height);
                let btn_rect = RectPx::new(
                    rect.left + row_px,
                    rect.top,
                    rect.right - row_px,
                    rect.top + px(26.0, s),
                );
                push_clipped_hit(hits, btn_rect, viewport_rect, action.clone(), CursorKind::Hand);
                rows.push(ConfigRowLayout {
                    rect,
                    kind: ConfigRowKind::ActionButton {
                        action,
                        label,
                        inverted,
                        btn_rect,
                    },
                });
                y += height + gap;
            }
            Spec::Divider { height } => {
                let rect = RectPx::from_xywh(inner_l, y, inner_w, height);
                let div_y = rect.top + height / 2;
                rows.push(ConfigRowLayout {
                    rect,
                    kind: ConfigRowKind::Divider {
                        y: div_y,
                        left: rect.left + px(4.0, s),
                        right: rect.right - px(4.0, s),
                    },
                });
                y += height + gap;
            }
            Spec::Quit { height } => {
                let rect = RectPx::from_xywh(inner_l, y, inner_w, height);
                let icon_s = px(13.0, s);
                let icon_rect = RectPx::from_xywh(
                    rect.left + row_px,
                    rect.top + (height - icon_s) / 2,
                    icon_s,
                    icon_s,
                );
                let label_rect = RectPx::new(
                    icon_rect.right + px(8.0, s),
                    rect.top,
                    rect.right - row_px,
                    rect.bottom,
                );
                push_clipped_hit(hits, rect, viewport_rect, HitAction::ConfigQuit, CursorKind::Hand);
                rows.push(ConfigRowLayout {
                    rect,
                    kind: ConfigRowKind::QuitRow {
                        icon_rect,
                        label_rect,
                        confirm_quit: input.confirm_quit,
                    },
                });
                y += height + gap;
            }
        }
    }

    ConfigOverlayLayout {
        panel_rect,
        viewport_rect,
        rows,
        max_scroll,
        scroll,
        content_height,
    }
}

fn compute_profile_overlay(
    input: &LayoutInput<'_>,
    anchor: RectPx,
    hits: &mut Vec<HitItem>,
) -> ProfileOverlayLayout {
    let s = input.scale;
    let margin = px(8.0, s);
    let width = anchor.width().max(px(204.0, s)).min(input.width - margin * 2);
    let left = anchor.left.clamp(margin, (input.width - margin - width).max(margin));
    let top = anchor.bottom + px(4.0, s);
    let max_h = px(250.0, s).min((input.height - top - margin).max(px(80.0, s)));

    let pad = px(4.0, s);
    let gap = px(2.0, s);
    let row_h = px(28.0, s);
    let empty_h = px(52.0, s);
    let count = input.state.profiles.len();
    let content_height = if count == 0 {
        empty_h
    } else {
        count as i32 * row_h + (count.saturating_sub(1) as i32) * gap
    };
    let panel_h = (content_height + pad * 2).min(max_h);
    let panel_rect = RectPx::from_xywh(left, top, width, panel_h);
    let viewport_rect = RectPx::new(
        panel_rect.left + pad,
        panel_rect.top + pad,
        panel_rect.right - pad,
        panel_rect.bottom - pad,
    );
    let max_scroll = (content_height - viewport_rect.height()).max(0);
    let scroll = input.profile_scroll.clamp(0, max_scroll);

    hits.push(HitItem {
        rect: RectPx::new(0, 0, input.width, input.height),
        action: HitAction::DismissOverlay,
        cursor: CursorKind::Arrow,
    });
    hits.push(HitItem {
        rect: panel_rect,
        action: HitAction::OverlaySurface,
        cursor: CursorKind::Arrow,
    });

    let mut rows = Vec::with_capacity(count);
    let empty_rect = if count == 0 {
        Some(RectPx::from_xywh(
            viewport_rect.left,
            viewport_rect.top,
            viewport_rect.width(),
            empty_h,
        ))
    } else {
        let active_id = input.state.active_profile_id.as_deref();
        let row_px = px(8.0, s);
        let chk_s = px(12.0, s);
        let del_s = px(18.0, s);
        let mut y = viewport_rect.top - scroll;

        for profile in &input.state.profiles {
            let row_rect = RectPx::from_xywh(viewport_rect.left, y, viewport_rect.width(), row_h);
            let check_rect = RectPx::from_xywh(
                row_rect.left + row_px,
                row_rect.top + (row_h - chk_s) / 2,
                chk_s,
                chk_s,
            );
            let delete_btn = RectPx::from_xywh(
                row_rect.right - row_px - del_s,
                row_rect.top + (row_h - del_s) / 2,
                del_s,
                del_s,
            );
            let label_rect = RectPx::new(
                check_rect.right + px(6.0, s),
                row_rect.top,
                (delete_btn.left - px(6.0, s)).max(check_rect.right),
                row_rect.bottom,
            );

            push_clipped_hit(
                hits,
                row_rect,
                viewport_rect,
                HitAction::ApplyProfile(profile.id.clone()),
                CursorKind::Hand,
            );
            push_clipped_hit(
                hits,
                delete_btn,
                viewport_rect,
                HitAction::DeleteProfile(profile.id.clone()),
                CursorKind::Hand,
            );

            rows.push(ProfileOptionLayout {
                id: profile.id.clone(),
                name: profile.name.clone(),
                active: active_id == Some(profile.id.as_str()),
                row_rect,
                check_rect,
                label_rect,
                delete_btn,
            });
            y += row_h + gap;
        }
        None
    };

    ProfileOverlayLayout {
        panel_rect,
        viewport_rect,
        rows,
        empty_rect,
        max_scroll,
        scroll,
        content_height,
    }
}

fn compute_listen_overlay(
    hdc: HDC,
    fonts: &FontCache,
    input: &LayoutInput<'_>,
    input_device_id: &str,
    anchor: Option<RectPx>,
    hits: &mut Vec<HitItem>,
) -> ListenOverlayLayout {
    let s = input.scale;
    let margin = px(LISTEN_POPOVER_MARGIN, s);
    let width = px(LISTEN_POPOVER_WIDTH, s).min(input.width - margin * 2);
    let max_pop_h = px(LISTEN_POPOVER_MAX_HEIGHT, s);

    let current_target = input
        .state
        .devices
        .iter()
        .find(|d| d.id == input_device_id)
        .and_then(|d| d.listen.as_ref())
        .and_then(|l| l.target_device_id.clone());

    let needle = input.listen_search_query.trim().to_ascii_lowercase();
    let matches_needle = |text: &str| text.to_ascii_lowercase().contains(&needle);

    let default_output = input
        .state
        .devices
        .iter()
        .find(|d| d.id == input.state.roles.default_output_id);
    let comms_output = input
        .state
        .devices
        .iter()
        .find(|d| d.id == input.state.roles.comms_output_id);

    let show_default = needle.is_empty()
        || matches_needle("Default Playback Device")
        || default_output
            .is_some_and(|d| matches_needle(&d.name) || matches_needle(&d.subtitle));
    let show_comms = needle.is_empty()
        || matches_needle("Default Communications Device")
        || comms_output
            .is_some_and(|d| matches_needle(&d.name) || matches_needle(&d.subtitle));
    let show_none = needle.is_empty() || matches_needle("None");

    let outputs: Vec<_> = input
        .state
        .devices_for(DeviceFlow::Output)
        .filter(|d| needle.is_empty() || matches_needle(&d.name) || matches_needle(&d.subtitle))
        .collect();

    let pad = px(6.0, s);
    let search_h = px(28.0, s);
    let header_gap = px(6.0, s);
    let row_gap = px(2.0, s);
    let bw = px(1.0, s).max(1);

    let def_entry_h = |has_sub: bool| -> i32 {
        if has_sub {
            px(6.0 * 2.0 + 15.0 + 1.0 + 13.0, s)
        } else {
            px(6.0 * 2.0 + 15.0, s)
        }
    };
    let dev_entry_h = |has_sub: bool| -> i32 {
        if has_sub {
            px(5.5 * 2.0 + 15.0 + 1.0 + 13.0, s)
        } else {
            px(5.5 * 2.0 + 15.0, s)
        }
    };

    let mut default_specs: Vec<(&'static str, &'static str, Option<String>)> = Vec::new();
    if show_default {
        default_specs.push((
            LISTEN_TARGET_DEFAULT_OUTPUT,
            "Default Playback Device",
            default_output.map(|d| d.name.clone()),
        ));
    }
    if show_comms {
        default_specs.push((
            LISTEN_TARGET_COMMS_OUTPUT,
            "Default Communications Device",
            comms_output.map(|d| d.name.clone()),
        ));
    }

    let has_default_section = !default_specs.is_empty();
    let has_lower_section = show_none || !outputs.is_empty();

    let mut content_h = 0i32;
    let default_box_h = if has_default_section {
        let mut h = bw * 2;
        for (idx, (_, _, sub)) in default_specs.iter().enumerate() {
            if idx > 0 {
                h += bw;
            }
            h += def_entry_h(sub.is_some());
        }
        content_h += h;
        h
    } else {
        0
    };

    let div_h = px(9.0, s);
    if has_default_section && has_lower_section {
        content_h += row_gap + div_h + row_gap;
    }

    let mut lower_count = 0usize;
    if show_none {
        if lower_count > 0 {
            content_h += row_gap;
        }
        content_h += dev_entry_h(false);
        lower_count += 1;
    }
    for out in &outputs {
        if lower_count > 0 {
            content_h += row_gap;
        }
        content_h += dev_entry_h(!out.subtitle.is_empty());
        lower_count += 1;
    }

    let empty_h = px(54.0, s);
    if !has_default_section && !has_lower_section {
        content_h = empty_h;
    }

    let desired_panel_h = pad * 2 + search_h + header_gap + content_h;

    let (left, top, panel_h) = match anchor {
        Some(bounds) => {
            let x = (bounds.right - width).clamp(margin, (input.width - width - margin).max(margin));
            let below_top = bounds.bottom + px(4.0, s);
            let space_below = (input.height - margin - below_top).max(0);
            let space_above = (bounds.top - px(4.0, s) - margin).max(0);
            if space_below >= px(220.0, s) || space_below >= space_above {
                let avail_h = space_below.clamp(px(120.0, s), max_pop_h);
                let h = desired_panel_h.min(avail_h);
                let y = below_top.min((input.height - margin - h).max(margin));
                (x, y, h)
            } else {
                let avail_h = space_above.clamp(px(120.0, s), max_pop_h);
                let h = desired_panel_h.min(avail_h);
                let y = (bounds.top - px(4.0, s) - h).clamp(margin, (input.height - margin - h).max(margin));
                (x, y, h)
            }
        }
        None => {
            let x = (input.width - width - margin).max(margin);
            let h = desired_panel_h.min(max_pop_h);
            let y = px(180.0, s).min((input.height - margin - h).max(margin));
            (x, y, h)
        }
    };

    let panel_rect = RectPx::from_xywh(left, top, width, panel_h);
    let inner_l = panel_rect.left + pad;
    let inner_r = panel_rect.right - pad;
    let inner_w = inner_r - inner_l;

    let search_box = RectPx::from_xywh(inner_l, panel_rect.top + pad, inner_w, search_h);
    let search_clear_btn = (!input.listen_search_query.is_empty()).then(|| {
        let bs = px(18.0, s);
        RectPx::from_xywh(
            search_box.right - px(5.0, s) - bs,
            search_box.top + (search_h - bs) / 2,
            bs,
            bs,
        )
    });

    let viewport_rect = RectPx::new(
        inner_l,
        search_box.bottom + header_gap,
        inner_r,
        panel_rect.bottom - pad,
    );
    let max_scroll = (content_h - viewport_rect.height()).max(0);
    let scroll = input.listen_scroll.clamp(0, max_scroll);

    hits.push(HitItem {
        rect: RectPx::new(0, 0, input.width, input.height),
        action: HitAction::DismissOverlay,
        cursor: CursorKind::Arrow,
    });
    hits.push(HitItem {
        rect: panel_rect,
        action: HitAction::OverlaySurface,
        cursor: CursorKind::Arrow,
    });
    hits.push(HitItem {
        rect: search_box,
        action: HitAction::FocusListenSearch,
        cursor: CursorKind::IBeam,
    });
    if let Some(clear_r) = search_clear_btn {
        hits.push(HitItem {
            rect: clear_r,
            action: HitAction::ClearListenSearch,
            cursor: CursorKind::Hand,
        });
    }

    let chk_s = px(12.0, s);
    let row_px = px(8.0, s);
    let chk_gap = px(8.0, s);
    let title_h = px(15.0, s);
    let sub_h = px(13.0, s);
    let r_inner = 6.0 * s;

    let mut y = viewport_rect.top - scroll;
    let mut default_section_rect = None;
    let mut default_divider_y = None;
    let mut default_entries = Vec::new();

    if has_default_section {
        let sec_rect = RectPx::from_xywh(inner_l, y, inner_w, default_box_h);
        default_section_rect = Some(sec_rect);
        let mut dy = sec_rect.top + bw;
        let entry_count = default_specs.len();

        for (idx, (target_const, title, subtext)) in default_specs.into_iter().enumerate() {
            if idx > 0 {
                default_divider_y = Some(dy);
                dy += bw;
            }
            let eh = def_entry_h(subtext.is_some());
            let rect = RectPx::new(sec_rect.left + bw, dy, sec_rect.right - bw, dy + eh);
            let check_rect = RectPx::from_xywh(
                rect.left + row_px,
                rect.top + (eh - chk_s) / 2,
                chk_s,
                chk_s,
            );
            let tl = check_rect.right + chk_gap;
            let tr = rect.right - row_px;
            let (title_rect, subtext_rect) = if subtext.is_some() {
                let top_t = rect.top + px(6.0, s);
                let t_rect = RectPx::new(tl, top_t, tr, top_t + title_h);
                let s_rect = RectPx::new(tl, t_rect.bottom + px(1.0, s), tr, t_rect.bottom + px(1.0, s) + sub_h);
                (t_rect, Some(s_rect))
            } else {
                (RectPx::new(tl, rect.top, tr, rect.bottom), None)
            };

            let is_first = idx == 0;
            let is_last = idx + 1 == entry_count;
            let radii = match (is_first, is_last) {
                (true, true) => [r_inner, r_inner, r_inner, r_inner],
                (true, false) => [r_inner, r_inner, 0.0, 0.0],
                (false, true) => [0.0, 0.0, r_inner, r_inner],
                (false, false) => [0.0; 4],
            };

            push_clipped_hit(
                hits,
                rect,
                viewport_rect,
                HitAction::SetListenTarget {
                    device_id: input_device_id.to_string(),
                    target_id: Some(target_const.to_string()),
                },
                CursorKind::Hand,
            );

            default_entries.push(ListenDefaultEntryLayout {
                target_id: target_const,
                title,
                subtext,
                active: current_target.as_deref() == Some(target_const),
                rect,
                check_rect,
                title_rect,
                subtext_rect,
                radii,
            });
            dy += eh;
        }
        y = sec_rect.bottom;
    }

    let section_divider_y = if has_default_section && has_lower_section {
        y += row_gap;
        let div_y = y + div_h / 2;
        y += div_h + row_gap;
        Some(div_y)
    } else {
        None
    };

    let mut device_entries = Vec::new();
    let disabled_pill_tw = measure_text_width(hdc, fonts.get(FontRole::Micro9_5Med), "Disabled");
    let disabled_pill_w = disabled_pill_tw + px(10.0, s);
    let disabled_pill_h = px(15.0, s);

    if show_none {
        let eh = dev_entry_h(false);
        let rect = RectPx::from_xywh(inner_l, y, inner_w, eh);
        let check_rect = RectPx::from_xywh(
            rect.left + row_px,
            rect.top + (eh - chk_s) / 2,
            chk_s,
            chk_s,
        );
        let title_rect = RectPx::new(
            check_rect.right + chk_gap,
            rect.top,
            rect.right - row_px,
            rect.bottom,
        );
        push_clipped_hit(
            hits,
            rect,
            viewport_rect,
            HitAction::SetListenTarget {
                device_id: input_device_id.to_string(),
                target_id: None,
            },
            CursorKind::Hand,
        );
        device_entries.push(ListenDeviceOptionLayout {
            target_id: None,
            title: "None".to_string(),
            subtitle: None,
            disabled: false,
            active: current_target.is_none(),
            rect,
            check_rect,
            title_rect,
            disabled_pill: None,
            subtitle_rect: None,
        });
        y += eh + row_gap;
    }

    for output in outputs {
        let sub = (!output.subtitle.is_empty()).then(|| output.subtitle.clone());
        let eh = dev_entry_h(sub.is_some());
        let rect = RectPx::from_xywh(inner_l, y, inner_w, eh);
        let check_rect = RectPx::from_xywh(
            rect.left + row_px,
            rect.top + (eh - chk_s) / 2,
            chk_s,
            chk_s,
        );
        let tl = check_rect.right + chk_gap;
        let tr = rect.right - row_px;
        let is_active = current_target.as_deref() == Some(output.id.as_str());

        let (title_top, title_bottom) = if sub.is_some() {
            let tt = rect.top + px(5.5, s);
            (tt, tt + title_h)
        } else {
            (rect.top, rect.bottom)
        };

        let (title_rect, disabled_pill) = if output.disabled {
            let name_font = if is_active {
                fonts.get(FontRole::Body11_5Semi)
            } else {
                fonts.get(FontRole::Body11_5Med)
            };
            let max_name_w = (tr - tl - px(6.0, s) - disabled_pill_w).max(px(40.0, s));
            let name_w = measure_text_width(hdc, name_font, &output.name).min(max_name_w);
            let t_rect = RectPx::new(tl, title_top, tl + name_w + px(2.0, s), title_bottom);
            let pill_x = (t_rect.right + px(6.0, s)).min(tr - disabled_pill_w);
            let pill_y = title_top + ((title_bottom - title_top) - disabled_pill_h) / 2;
            let p_rect = RectPx::from_xywh(pill_x, pill_y, disabled_pill_w, disabled_pill_h);
            (t_rect, Some(p_rect))
        } else {
            (RectPx::new(tl, title_top, tr, title_bottom), None)
        };

        let subtitle_rect = sub.as_ref().map(|_| {
            RectPx::new(tl, title_bottom + px(1.0, s), tr, title_bottom + px(1.0, s) + sub_h)
        });

        push_clipped_hit(
            hits,
            rect,
            viewport_rect,
            HitAction::SetListenTarget {
                device_id: input_device_id.to_string(),
                target_id: Some(output.id.clone()),
            },
            CursorKind::Hand,
        );

        device_entries.push(ListenDeviceOptionLayout {
            target_id: Some(output.id.clone()),
            title: output.name.clone(),
            subtitle: sub,
            disabled: output.disabled,
            active: is_active,
            rect,
            check_rect,
            title_rect,
            disabled_pill,
            subtitle_rect,
        });
        y += eh + row_gap;
    }

    let empty_rect = (!has_default_section && !has_lower_section).then(|| {
        RectPx::from_xywh(inner_l, viewport_rect.top, inner_w, empty_h)
    });

    ListenOverlayLayout {
        input_device_id: input_device_id.to_string(),
        panel_rect,
        search_box,
        search_clear_btn,
        viewport_rect,
        default_section_rect,
        default_divider_y,
        default_entries,
        section_divider_y,
        device_entries,
        empty_rect,
        max_scroll,
        scroll,
        content_height: content_h,
    }
}

fn push_clipped_hit(
    hits: &mut Vec<HitItem>,
    rect: RectPx,
    clip: RectPx,
    action: HitAction,
    cursor: CursorKind,
) {
    let clipped = rect.intersect(clip);
    if !clipped.is_empty() {
        hits.push(HitItem {
            rect: clipped,
            action,
            cursor,
        });
    }
}
