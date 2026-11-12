//! The saved description of a production (a "preset"): inputs, outputs,
//! transitions, audio routing. Everything here is plain data.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum TitleStyle {
    #[default]
    LowerThird,
    Centered,
    Ticker,
    Countdown,
}

/// A shape inside a designed title.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum TitleObject {
    Text { text: String, x: f32, y: f32, size: f32, color: [u8; 3], bold: bool },
    Rect { x: f32, y: f32, w: f32, h: f32, color: [u8; 3], alpha: u8 },
    Image { path: String, x: f32, y: f32, w: f32, h: f32 },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct TitlePreset {
    pub name: String,
    pub text: String,
    pub sub: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ListItem {
    pub path: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Kind {
    Bars,
    Motion,
    Clock,
    Tone,
    Solid {
        color: [u8; 3],
    },
    Image {
        path: String,
    },
    Video {
        path: String,
        looped: bool,
        mark_in: u64,
        mark_out: u64,
        rate: f32,
    },
    AudioFile {
        path: String,
        looped: bool,
    },
    List {
        items: Vec<ListItem>,
        auto_next: bool,
        auto_first: bool,
        looped: bool,
        shuffle: bool,
    },
    Photos {
        folder: String,
        seconds: f32,
    },
    ImageSequence {
        first: String,
        fps: f32,
        looped: bool,
    },
    /// A DVD-Video title: a disc drive, a folder holding VIDEO_TS, or an .iso.
    Dvd {
        #[serde(default)]
        path: String,
        /// Counted from 1, as on the disc.
        #[serde(default)]
        title: u32,
    },
    /// A presentation; its slides are turned into pictures by PowerPoint or
    /// LibreOffice, whichever the machine has.
    PowerPoint {
        path: String,
    },
    Camera {
        device: String,
        ptz_host: String,
    },
    Network {
        url: String,
        buffer_ms: u32,
    },
    /// An NDI source on the network, by its full name: `MACHINE (Name)`.
    Ndi {
        source: String,
    },
    /// An OMT source on the network, by its full name or an omt:// address.
    Omt {
        source: String,
    },
    /// A web page; lpha keeps its transparent background.
    Browser {
        url: String,
        alpha: bool,
    },
    Screen,
    Microphone {
        device: String,
    },
    SystemAudio,
    Title {
        text: String,
        sub: String,
        style: TitleStyle,
        color: [u8; 3],
        accent: [u8; 3],
        ticker_speed: f32,
        countdown_ms: u64,
        /// Family name of an installed font; empty uses the built-in one.
        #[serde(default)]
        font: String,
        /// Text height in pixels on a 1080-line frame; 0 picks one.
        #[serde(default)]
        size: f32,
        #[serde(default)]
        presets: Vec<TitlePreset>,
        /// Data source row binding: column for the text, column for the sub.
        #[serde(default)]
        data: Option<(usize, usize, usize)>,
    },
    Designed {
        objects: Vec<TitleObject>,
        anim_ms: u32,
    },
    /// Nothing of its own; shows its layers.
    Scene,
    /// A small second mixer, cutting or fading between two inputs.
    Mix {
        preview: Option<u64>,
        output: Option<u64>,
        ms: u32,
    },
    Replay,
    Telestrator {
        background: Option<u64>,
    },
    /// Presenters standing in a decor, seen through a camera that moves
    /// between presets. Its layers are the presenters, placed in the set.
    VirtualSet {
        #[serde(default)]
        look: SetLook,
        /// Replaces the built-in backdrop when chosen.
        #[serde(default)]
        background: Option<u64>,
        /// Replaces the built-in desk, drawn in front of the presenters.
        #[serde(default)]
        foreground: Option<u64>,
        #[serde(default)]
        cameras: Vec<SetCamera>,
        /// The preset the camera stands on, or is moving to.
        #[serde(default)]
        camera: usize,
        #[serde(default)]
        move_ms: u32,
    },
    /// A remote guest's camera and microphone, sent from their own browser.
    VideoCall {
        /// The secret part of the guest's link. A new one locks the old link out.
        #[serde(default)]
        token: String,
        /// Name or address put in the link for guests reaching this computer
        /// from outside; empty uses its address on the local network.
        #[serde(default)]
        address: String,
        #[serde(default)]
        stun: String,
        #[serde(default)]
        turn: String,
        #[serde(default)]
        turn_user: String,
        #[serde(default)]
        turn_password: String,
        #[serde(default)]
        return_sound: ReturnSound,
    },
}

/// What a video call guest hears back, along with the program picture.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum ReturnSound {
    /// The main mix without the guest, who would otherwise hear their own
    /// voice come back late.
    #[default]
    MasterMinusGuest,
    /// One bus, 1 to 7 for A to G: a mix built for the guest.
    Bus(u8),
    Off,
}

impl ReturnSound {
    pub fn all() -> Vec<ReturnSound> {
        let mut v = vec![ReturnSound::MasterMinusGuest];
        v.extend((1..=7).map(ReturnSound::Bus));
        v.push(ReturnSound::Off);
        v
    }

    pub fn label(self) -> String {
        match self {
            ReturnSound::MasterMinusGuest => t!("model.return.master_minus_guest").into(),
            ReturnSound::Bus(n) => {
                t!("model.return.bus", name = "ABCDEFG".chars().nth(n as usize - 1).unwrap_or('?'))
            }
            ReturnSound::Off => t!("model.return.off").into(),
        }
    }
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Bars => t!("model.kind.bars"),
            Kind::Motion => t!("model.kind.motion"),
            Kind::Clock => t!("model.kind.clock"),
            Kind::Tone => t!("model.kind.tone"),
            Kind::Solid { .. } => t!("model.kind.solid"),
            Kind::Image { .. } => t!("model.kind.image"),
            Kind::Video { .. } => t!("model.kind.video"),
            Kind::AudioFile { .. } => t!("model.kind.audio_file"),
            Kind::List { .. } => t!("model.kind.list"),
            Kind::Photos { .. } => t!("model.kind.photos"),
            Kind::ImageSequence { .. } => t!("model.kind.image_sequence"),
            Kind::Dvd { .. } => t!("model.kind.dvd"),
            Kind::PowerPoint { .. } => t!("model.kind.powerpoint"),
            Kind::Camera { .. } => t!("model.kind.camera"),
            Kind::Network { .. } => t!("model.kind.network"),
            Kind::Ndi { .. } => "NDI",
            Kind::Omt { .. } => "OMT",
            Kind::Browser { .. } => t!("model.kind.browser"),
            Kind::Screen => t!("model.kind.screen"),
            Kind::Microphone { .. } => t!("model.kind.microphone"),
            Kind::SystemAudio => t!("model.kind.system_audio"),
            Kind::Title { .. } => t!("model.kind.title"),
            Kind::Designed { .. } => t!("model.kind.designed"),
            Kind::Scene => t!("model.kind.scene"),
            Kind::Mix { .. } => t!("model.kind.mix"),
            Kind::Replay => t!("model.kind.replay"),
            Kind::Telestrator { .. } => t!("model.kind.telestrator"),
            Kind::VirtualSet { .. } => t!("model.kind.virtual_set"),
            Kind::VideoCall { .. } => t!("model.kind.video_call"),
        }
    }

    /// Name the XML state gives this kind of input.
    pub fn api_type(&self) -> &'static str {
        match self {
            Kind::Bars | Kind::Solid { .. } | Kind::Scene | Kind::Motion => "Colour",
            Kind::Clock => "Xaml",
            Kind::Tone | Kind::Microphone { .. } | Kind::SystemAudio => "AudioInput",
            Kind::Image { .. } => "Image",
            Kind::Video { .. } => "Video",
            Kind::AudioFile { .. } => "AudioFile",
            Kind::List { .. } => "VideoList",
            Kind::Photos { .. } => "Photos",
            Kind::ImageSequence { .. } => "ImageSequence",
            Kind::Dvd { .. } => "DVD",
            Kind::PowerPoint { .. } => "PowerPoint",
            Kind::Camera { .. } => "Capture",
            Kind::Network { .. } => "Stream",
            Kind::Ndi { .. } => "NDI",
            Kind::Omt { .. } => "OMT",
            Kind::Browser { .. } => "Browser",
            Kind::Screen => "Desktop",
            Kind::Title { .. } | Kind::Designed { .. } => "GT",
            Kind::Mix { .. } => "Mix",
            Kind::Replay => "Replay",
            Kind::Telestrator { .. } => "Telestrator",
            Kind::VirtualSet { .. } => "VirtualSet",
            Kind::VideoCall { .. } => "VideoCall",
        }
    }

    pub fn has_audio(&self) -> bool {
        matches!(
            self,
            Kind::Video { .. }
                | Kind::AudioFile { .. }
                | Kind::List { .. }
                | Kind::Dvd { .. }
                | Kind::Network { .. }
                | Kind::Ndi { .. }
                | Kind::Omt { .. }
                | Kind::Microphone { .. }
                | Kind::SystemAudio
                | Kind::Tone
                | Kind::VideoCall { .. }
        )
    }

    pub fn has_picture(&self) -> bool {
        !matches!(self, Kind::Microphone { .. } | Kind::SystemAudio | Kind::Tone | Kind::AudioFile { .. })
    }

    /// Has a timeline: position, restart, play and pause apply.
    pub fn has_timeline(&self) -> bool {
        matches!(
            self,
            Kind::Video { .. }
                | Kind::AudioFile { .. }
                | Kind::List { .. }
                | Kind::ImageSequence { .. }
                | Kind::Dvd { .. }
        )
    }

    /// Still the same media: settings changed, but where it stood means
    /// something in the new version too.
    pub fn same_source(&self, other: &Kind) -> bool {
        match (self, other) {
            (Kind::Video { path: a, .. }, Kind::Video { path: b, .. })
            | (Kind::AudioFile { path: a, .. }, Kind::AudioFile { path: b, .. })
            | (Kind::ImageSequence { first: a, .. }, Kind::ImageSequence { first: b, .. })
            | (Kind::PowerPoint { path: a }, Kind::PowerPoint { path: b }) => a == b,
            (Kind::List { items: a, .. }, Kind::List { items: b, .. }) => a == b,
            // A position only means something within the same title.
            (Kind::Dvd { path: a, title: x }, Kind::Dvd { path: b, title: y }) => a == b && x == y,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    /// Whether a running source must start again after this change. A call
    /// keeps its guest while only the link's address or the return changes.
    pub fn restarts_source(&self, before: &Kind) -> bool {
        match (self, before) {
            (
                Kind::VideoCall { token, stun, turn, turn_user, turn_password, .. },
                Kind::VideoCall { token: t, stun: s, turn: u, turn_user: n, turn_password: p, .. },
            ) => (token, stun, turn, turn_user, turn_password) != (t, s, u, n, p),
            _ => self != before,
        }
    }

    /// Live pictures worth keeping as separate recordings.
    pub fn is_recordable(&self) -> bool {
        matches!(
            self,
            Kind::Camera { .. }
                | Kind::Network { .. }
                | Kind::Ndi { .. }
                | Kind::Omt { .. }
                | Kind::Screen
                | Kind::Video { .. }
                | Kind::VideoCall { .. }
        )
    }
}

/// Where a picture lands, as fractions of the output.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
}

impl Placement {
    pub const FULL: Placement = Placement { x: 0.0, y: 0.0, scale: 1.0 };

    pub fn is_full(&self) -> bool {
        self.scale >= 0.999 && self.x.abs() < 0.001 && self.y.abs() < 0.001
    }

    pub fn lerp(self, o: Placement, t: f32) -> Placement {
        let m = |a: f32, b: f32| a + (b - a) * t;
        Placement { x: m(self.x, o.x), y: m(self.y, o.y), scale: m(self.scale, o.scale) }
    }
}

/// Decors drawn by LiveCraft itself, so a set works without any file.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum SetLook {
    #[default]
    Studio,
    Evening,
    /// Only the pictures chosen for the background and foreground.
    Empty,
}

/// What a virtual set's camera shows: `zoom` 1 is the whole set, `x` and `y`
/// the centre of the view as fractions of the set.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct SetView {
    pub zoom: f32,
    pub x: f32,
    pub y: f32,
}

impl Default for SetView {
    fn default() -> Self {
        Self { zoom: 1.0, x: 0.5, y: 0.5 }
    }
}

/// A named camera preset of a virtual set.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct SetCamera {
    pub name: String,
    pub view: SetView,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Border {
    pub color: [u8; 3],
    pub thickness: f32,
    pub radius: f32,
}

impl Default for Border {
    fn default() -> Self {
        Self { color: [255, 255, 255], thickness: 0.0, radius: 0.0 }
    }
}

/// One of up to ten pictures shown on top of an input.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Layer {
    pub input: u64,
    pub visible: bool,
    pub placement: Placement,
    /// Left, right, top, bottom, as fractions of the source to remove.
    pub crop: [f32; 4],
    pub border: Border,
}

impl Layer {
    pub fn new(input: u64, placement: Placement) -> Self {
        Self { input, visible: true, placement, crop: [0.0; 4], border: Border::default() }
    }
}

/// Input Settings: Position.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Position {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
    pub rotate: f32,
    pub crop: [f32; 4],
    /// Quarter turns applied first, for cameras mounted sideways.
    pub quarter_turns: u8,
}

impl Default for Position {
    fn default() -> Self {
        Self { zoom: 1.0, pan_x: 0.0, pan_y: 0.0, rotate: 0.0, crop: [0.0; 4], quarter_turns: 0 }
    }
}

impl Position {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// Input Settings: Colour Adjust and Colour Correction together.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Colour {
    pub black: f32,
    pub white: f32,
    pub rgb: [f32; 3],
    pub alpha: f32,
    pub saturation: f32,
    pub hue: f32,
    pub lift: [f32; 3],
    pub gamma: [f32; 3],
    pub gain: [f32; 3],
}

impl Default for Colour {
    fn default() -> Self {
        Self {
            black: 0.0,
            white: 1.0,
            rgb: [1.0; 3],
            alpha: 1.0,
            saturation: 1.0,
            hue: 0.0,
            lift: [0.0; 3],
            gamma: [1.0; 3],
            gain: [1.0; 3],
        }
    }
}

impl Colour {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum KeyMode {
    #[default]
    Off,
    Colour,
    Chroma,
    Luma,
    Fill,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Key {
    pub mode: KeyMode,
    pub color: [u8; 3],
    pub tolerance: f32,
    pub softness: f32,
    /// Removes the coloured halo left on edges.
    pub spill: f32,
    pub fill_input: Option<u64>,
}

impl Default for Key {
    fn default() -> Self {
        Self {
            mode: KeyMode::Off,
            color: [0, 255, 0],
            tolerance: 0.25,
            softness: 0.1,
            spill: 0.5,
            fill_input: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum EffectKind {
    #[default]
    Blur,
    Sharpen,
    Lut,
    AlphaMask,
}

impl EffectKind {
    pub const ALL: [EffectKind; 4] =
        [EffectKind::Blur, EffectKind::Sharpen, EffectKind::Lut, EffectKind::AlphaMask];

    pub fn label(self) -> &'static str {
        match self {
            EffectKind::Blur => t!("model.effect.blur"),
            EffectKind::Sharpen => t!("model.effect.sharpen"),
            EffectKind::Lut => t!("model.effect.lut"),
            EffectKind::AlphaMask => t!("model.effect.alpha_mask"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Effect {
    pub kind: EffectKind,
    pub enabled: bool,
    pub strength: f32,
    pub path: String,
    /// Limits the effect to part of the picture. Left, right, top, bottom.
    pub crop: [f32; 4],
}

/// A third-party sound plug-in in a chain, with its own saved settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct PluginSlot {
    /// Follows the slot when the chain is reordered, so the running plug-in
    /// stays the same one.
    pub key: u64,
    /// The CLAP file, and the plug-in's identifier inside it.
    pub path: String,
    pub id: String,
    pub name: String,
    pub bypass: bool,
    /// The plug-in's own state, in base64. Only the plug-in reads it.
    pub state: String,
}

/// Per-input sound settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AudioCfg {
    pub volume: f32,
    pub balance: f32,
    pub muted: bool,
    /// Automatic mixing: heard only while on air.
    pub follow: bool,
    /// Bit 0 is Master, bits 1 to 7 are buses A to G.
    pub buses: u8,
    pub bus_levels: [f32; 8],
    pub gain_db: f32,
    pub delay_ms: f32,
    /// 0 stereo, 1 left only, 2 right only.
    pub channel: u8,
    pub headphones: f32,
    pub eq: [f32; 10],
    pub comp_ratio: f32,
    pub comp_threshold: f32,
    pub gate_threshold: f32,
    pub visible: bool,
    pub pfl: bool,
    /// Run after the built-in treatment, before the fader.
    pub plugins: Vec<PluginSlot>,
}

impl AudioCfg {
    /// Everything but the plug-ins: the sound thread copies these settings on
    /// every block, and plug-in states can be large.
    pub fn without_plugins(&self) -> AudioCfg {
        AudioCfg { plugins: Vec::new(), ..*self }
    }
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            volume: 1.0,
            balance: 0.0,
            muted: false,
            follow: true,
            buses: 1,
            bus_levels: [1.0; 8],
            gain_db: 0.0,
            delay_ms: 0.0,
            channel: 0,
            headphones: 1.0,
            eq: [0.0; 10],
            comp_ratio: 1.0,
            comp_threshold: -20.0,
            gate_threshold: -90.0,
            visible: true,
            pfl: false,
            plugins: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub enum GoAction {
    #[default]
    QuickPlay,
    Transition(usize),
    Shortcut {
        function: String,
        value: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum ClickAction {
    #[default]
    Preview,
    QuickPlay,
    Cut,
    Overlay(usize),
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum TriggerEvent {
    #[default]
    OnTransitionIn,
    OnTransitionOut,
    OnOverlayIn,
    OnOverlayOut,
    OnCompletion,
    OnCountdownCompleted,
    OnCountdownTime,
    OnCountdownRemaining,
    OnPlaybackTime,
    OnPlaybackRemaining,
}

impl TriggerEvent {
    pub const ALL: [TriggerEvent; 10] = [
        TriggerEvent::OnTransitionIn,
        TriggerEvent::OnTransitionOut,
        TriggerEvent::OnOverlayIn,
        TriggerEvent::OnOverlayOut,
        TriggerEvent::OnCompletion,
        TriggerEvent::OnCountdownCompleted,
        TriggerEvent::OnCountdownTime,
        TriggerEvent::OnCountdownRemaining,
        TriggerEvent::OnPlaybackTime,
        TriggerEvent::OnPlaybackRemaining,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TriggerEvent::OnTransitionIn => t!("model.trigger.transition_in"),
            TriggerEvent::OnTransitionOut => t!("model.trigger.transition_out"),
            TriggerEvent::OnOverlayIn => t!("model.trigger.overlay_in"),
            TriggerEvent::OnOverlayOut => t!("model.trigger.overlay_out"),
            TriggerEvent::OnCompletion => t!("model.trigger.completion"),
            TriggerEvent::OnCountdownCompleted => t!("model.trigger.countdown_completed"),
            TriggerEvent::OnCountdownTime => t!("model.trigger.countdown_time"),
            TriggerEvent::OnCountdownRemaining => t!("model.trigger.countdown_remaining"),
            TriggerEvent::OnPlaybackTime => t!("model.trigger.playback_time"),
            TriggerEvent::OnPlaybackRemaining => t!("model.trigger.playback_remaining"),
        }
    }

    pub fn uses_time(self) -> bool {
        matches!(
            self,
            TriggerEvent::OnCountdownTime
                | TriggerEvent::OnCountdownRemaining
                | TriggerEvent::OnPlaybackTime
                | TriggerEvent::OnPlaybackRemaining
        )
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Trigger {
    pub event: TriggerEvent,
    pub function: String,
    pub input: String,
    pub value: String,
    pub delay_ms: u32,
    pub time_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Input {
    pub id: u64,
    pub name: String,
    pub kind: Kind,
    pub audio: AudioCfg,
    pub position: Position,
    pub colour: Colour,
    pub key: Key,
    pub effects: Vec<Effect>,
    pub layers: Vec<Layer>,
    pub mirror: bool,
    /// Index into the project's categories; none shows only under View All.
    pub category: Option<usize>,
    pub compact: bool,
    pub go: GoAction,
    pub click: ClickAction,
    pub auto_play: bool,
    pub auto_restart: bool,
    pub auto_pause: bool,
    pub triggers: Vec<Trigger>,
}

impl Default for Input {
    fn default() -> Self {
        Self::new(0, "", Kind::Scene)
    }
}

impl Input {
    /// Writes back only what an editor changed from `before`: the desk or a
    /// remote control may have moved something else in the meantime.
    pub fn merge_changes(&mut self, before: &Input, after: Input) {
        macro_rules! changed {
            ($($field:ident),*) => {
                $(if after.$field != before.$field {
                    self.$field = after.$field;
                })*
            };
        }
        changed!(
            name,
            kind,
            audio,
            position,
            colour,
            key,
            effects,
            layers,
            mirror,
            category,
            compact,
            go,
            click,
            auto_play,
            auto_restart,
            auto_pause,
            triggers
        );
    }

    pub fn new(id: u64, name: impl Into<String>, kind: Kind) -> Self {
        let live_audio = matches!(kind, Kind::Microphone { .. } | Kind::Tone | Kind::SystemAudio);
        let audio = AudioCfg { follow: !live_audio, ..AudioCfg::default() };
        Self {
            id,
            name: name.into(),
            kind,
            audio,
            position: Position::default(),
            colour: Colour::default(),
            key: Key::default(),
            effects: Vec::new(),
            layers: Vec::new(),
            mirror: false,
            category: None,
            compact: false,
            go: GoAction::QuickPlay,
            click: ClickAction::Preview,
            auto_play: true,
            auto_restart: false,
            auto_pause: true,
            triggers: Vec::new(),
        }
    }

    /// Category numbers in presets start at 1, 0 meaning none.
    pub fn set_preset_category(&mut self, n: usize) {
        self.category = n.checked_sub(1);
    }

    pub fn in_category(mut self, category: usize) -> Self {
        self.category = Some(category);
        self
    }

    pub fn folded(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn with_layers(mut self, layers: Vec<Layer>) -> Self {
        self.layers = layers;
        self
    }

    /// Stable identifier for remote control, shaped like a GUID.
    pub fn key_id(&self) -> String {
        format!("00000000-0000-4000-8000-{:012x}", self.id)
    }

    /// Anything that changes the picture from what the source delivers.
    pub fn needs_processing(&self) -> bool {
        self.mirror
            || !self.position.is_identity()
            || !self.colour.is_identity()
            || self.key.mode != KeyMode::Off
            || self.effects.iter().any(|e| e.enabled)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum TransKind {
    Cut,
    Fade,
    AlphaFade,
    Merge,
    Zoom,
    Wipe,
    WipeReverse,
    VerticalWipe,
    VerticalWipeReverse,
    Slide,
    SlideReverse,
    VerticalSlide,
    VerticalSlideReverse,
    Fly,
    FlyRotate,
    CrossZoom,
    Cube,
    CubeZoom,
    BarnDoor,
    RollerDoor,
    Stinger(usize),
}

impl TransKind {
    pub const ALL: [TransKind; 20] = [
        TransKind::Cut,
        TransKind::Fade,
        TransKind::AlphaFade,
        TransKind::Merge,
        TransKind::Zoom,
        TransKind::Wipe,
        TransKind::WipeReverse,
        TransKind::VerticalWipe,
        TransKind::VerticalWipeReverse,
        TransKind::Slide,
        TransKind::SlideReverse,
        TransKind::VerticalSlide,
        TransKind::VerticalSlideReverse,
        TransKind::Fly,
        TransKind::FlyRotate,
        TransKind::CrossZoom,
        TransKind::Cube,
        TransKind::CubeZoom,
        TransKind::BarnDoor,
        TransKind::RollerDoor,
    ];

    pub fn label(self) -> String {
        let s = match self {
            TransKind::Cut => t!("model.trans.cut"),
            TransKind::Fade => t!("model.trans.fade"),
            TransKind::AlphaFade => t!("model.trans.alpha_fade"),
            TransKind::Merge => t!("model.trans.merge"),
            TransKind::Zoom => t!("model.trans.zoom"),
            TransKind::Wipe => t!("model.trans.wipe"),
            TransKind::WipeReverse => t!("model.trans.wipe_reverse"),
            TransKind::VerticalWipe => t!("model.trans.vertical_wipe"),
            TransKind::VerticalWipeReverse => t!("model.trans.vertical_wipe_reverse"),
            TransKind::Slide => t!("model.trans.slide"),
            TransKind::SlideReverse => t!("model.trans.slide_reverse"),
            TransKind::VerticalSlide => t!("model.trans.vertical_slide"),
            TransKind::VerticalSlideReverse => t!("model.trans.vertical_slide_reverse"),
            TransKind::Fly => t!("model.trans.fly"),
            TransKind::FlyRotate => t!("model.trans.fly_rotate"),
            TransKind::CrossZoom => t!("model.trans.cross_zoom"),
            TransKind::Cube => t!("model.trans.cube"),
            TransKind::CubeZoom => t!("model.trans.cube_zoom"),
            TransKind::BarnDoor => t!("model.trans.barn_door"),
            TransKind::RollerDoor => t!("model.trans.roller_door"),
            TransKind::Stinger(n) => return t!("model.trans.stinger", n = n + 1),
        };
        s.to_string()
    }

    /// Function name used by shortcuts and remote control.
    pub fn api_name(self) -> String {
        match self {
            TransKind::Stinger(n) => format!("Stinger{}", n + 1),
            other => format!("{other:?}"),
        }
    }

    pub fn from_api(name: &str) -> Option<TransKind> {
        let number = name.get(..7).filter(|p| p.eq_ignore_ascii_case("Stinger")).map(|_| &name[7..]);
        if let Some(n) = number.and_then(|v| v.parse::<usize>().ok()) {
            return (1..=8).contains(&n).then_some(TransKind::Stinger(n - 1));
        }
        TransKind::ALL.into_iter().find(|k| format!("{k:?}").eq_ignore_ascii_case(name))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct TransSlot {
    pub kind: TransKind,
    pub ms: u32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Category {
    pub name: String,
    pub color: [u8; 3],
    pub enabled: bool,
}

/// How one of the eight overlay channels shows its input.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct OverlayCfg {
    pub pip: bool,
    pub placement: Placement,
    pub effect: TransKind,
    pub effect_ms: u32,
    /// Closes by itself after this long; 0 stays on.
    pub duration_ms: u32,
    pub border: Border,
}

impl Default for OverlayCfg {
    fn default() -> Self {
        Self {
            pip: false,
            placement: Placement { x: 0.66, y: 0.05, scale: 0.3 },
            effect: TransKind::Fade,
            effect_ms: 500,
            duration_ms: 0,
            border: Border::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct StingerCfg {
    pub input: Option<u64>,
    pub duration_ms: u32,
    pub cut_ms: u32,
    pub under_overlays: bool,
}

impl Default for StingerCfg {
    fn default() -> Self {
        Self { input: None, duration_ms: 2000, cut_ms: 1000, under_overlays: false }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PlaylistItem {
    pub input: u64,
    pub start_ms: Option<u64>,
    /// 0 uses the input's own length.
    pub duration_ms: u64,
    pub trans: TransKind,
    pub trans_ms: u32,
    pub overlay: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Playlist {
    pub items: Vec<PlaylistItem>,
    pub looped: bool,
    pub manual: bool,
    pub clear_overlays: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum ClockType {
    #[default]
    Clock,
    Event,
    Recording,
    Streaming,
    TimeRemaining,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ClockCfg {
    pub kind: ClockType,
    pub inverted: bool,
    /// 0 minutes, 1 seconds, 2 tenths.
    pub precision: u8,
    pub hour_offset: i32,
    pub am_pm: bool,
    /// "HH:MM" for Event clocks.
    pub start: String,
    pub end: String,
}

impl ClockCfg {
    fn new(kind: ClockType, precision: u8) -> Self {
        Self {
            kind,
            inverted: false,
            precision,
            hour_offset: 0,
            am_pm: false,
            start: String::new(),
            end: String::new(),
        }
    }
}

/// Which sound an output carries.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct AudioPick {
    /// Bus numbers, 0 Master, 1 to 7 A to G. Several make a multi-track output.
    pub buses: [u8; 4],
    pub count: u8,
}

impl AudioPick {
    pub const MASTER: AudioPick = AudioPick { buses: [0; 4], count: 1 };

    /// The audio selections offered: single buses and the paired ones.
    pub fn all() -> Vec<(String, AudioPick)> {
        let mut v = vec![(t!("model.audio_pick.master").to_string(), Self::MASTER)];
        for (n, c) in "ABCDEFG".chars().enumerate() {
            v.push((c.to_string(), AudioPick { buses: [n as u8 + 1, 0, 0, 0], count: 1 }));
        }
        for (name, list) in [
            ("AB", &[1u8, 2][..]),
            ("CD", &[3, 4]),
            ("DE", &[4, 5]),
            ("ABCD", &[1, 2, 3, 4]),
            ("DEFG", &[4, 5, 6, 7]),
        ] {
            let mut buses = [0u8; 4];
            buses[..list.len()].copy_from_slice(list);
            v.push((name.to_string(), AudioPick { buses, count: list.len() as u8 }));
        }
        v
    }

    pub fn label(&self) -> String {
        Self::all()
            .into_iter()
            .find(|(_, p)| p == self)
            .map_or(t!("model.audio_pick.master").into(), |(n, _)| n)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RecordCfg {
    pub enabled: bool,
    /// mp4, mkv, mov, ts
    pub container: String,
    pub codec: String,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub audio: AudioPick,
    pub folder: String,
    /// Starts a new file every so many minutes; 0 never.
    pub split_minutes: u32,
}

impl RecordCfg {
    fn new(enabled: bool, video_kbps: u32) -> Self {
        Self {
            enabled,
            container: "mp4".into(),
            codec: "libx264".into(),
            video_kbps,
            audio_kbps: 192,
            audio: AudioPick::MASTER,
            folder: String::new(),
            split_minutes: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct StreamCfg {
    pub enabled: bool,
    pub url: String,
    pub key: String,
    /// Shares the encoder of destination 1 instead of its own quality.
    pub shared: bool,
    pub kbps: u32,
    pub audio_kbps: u32,
    pub codec: String,
}

impl Default for StreamCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "rtmp://".into(),
            key: String::new(),
            shared: true,
            kbps: 6000,
            audio_kbps: 160,
            codec: "libx264".into(),
        }
    }
}

/// Picture sent to a screen or network output.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum Route {
    #[default]
    Output,
    Preview,
    MultiView,
    MultiView2,
    Input(u64),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SrtOut {
    pub enabled: bool,
    pub route: Route,
    /// Caller, listener or rendezvous, as an srt:// address.
    pub url: String,
    pub kbps: u32,
    pub audio: AudioPick,
}

impl Default for SrtOut {
    fn default() -> Self {
        Self {
            enabled: false,
            route: Route::Output,
            url: "srt://127.0.0.1:9000?mode=caller".into(),
            kbps: 6000,
            audio: AudioPick::MASTER,
        }
    }
}

/// The program with Master sound, offered on the network as an NDI or OMT source.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct NetOutCfg {
    pub enabled: bool,
    /// What receivers see after the computer's name.
    pub name: String,
}

impl Default for NetOutCfg {
    fn default() -> Self {
        Self { enabled: false, name: "LiveCraft".into() }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct BusCfg {
    pub level: f32,
    pub muted: bool,
    pub send_to_master: bool,
    pub device: String,
    /// Run on the summed bus, before its level.
    pub plugins: Vec<PluginSlot>,
}

impl Default for BusCfg {
    fn default() -> Self {
        Self { level: 1.0, muted: false, send_to_master: false, device: String::new(), plugins: Vec::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Shortcut {
    pub key: String,
    pub function: String,
    pub input: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Script {
    pub name: String,
    pub code: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct DataSource {
    pub name: String,
    /// A CSV or plain text file, read again whenever it changes.
    pub path: String,
    pub row: usize,
    pub auto_next_secs: f32,
}

/// Saved window of Layer positions, reusable across projects.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct LayerTemplate {
    pub name: String,
    pub layers: Vec<Layer>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Project {
    pub inputs: Vec<Input>,
    pub preview: Option<u64>,
    pub program: Option<u64>,
    pub overlays: [Option<u64>; 8],
    /// What each channel last showed, so switching it back on brings the same input.
    pub overlay_last: [Option<u64>; 8],
    pub overlay_cfg: [OverlayCfg; 8],
    pub stingers: [StingerCfg; 8],
    pub trans: [TransSlot; 4],
    pub quick_play: TransSlot,
    pub categories: Vec<Category>,
    pub out_w: usize,
    pub out_h: usize,
    pub fps: u32,
    pub playlist: Playlist,
    pub clocks: [ClockCfg; 2],
    pub show_clocks: bool,
    pub records: [RecordCfg; 2],
    pub streams: [StreamCfg; 5],
    pub srt: [SrtOut; 4],
    pub ndi_out: NetOutCfg,
    pub omt_out: NetOutCfg,
    pub master_level: f32,
    pub master_muted: bool,
    pub buses: [BusCfg; 7],
    pub master_device: String,
    /// Run on the Master sum, before its level.
    pub master_plugins: Vec<PluginSlot>,
    pub headphones_device: String,
    pub headphones_level: f32,
    pub ftb_audio: bool,
    pub fullscreen_route: [Route; 2],
    pub fullscreen_monitor: [usize; 2],
    pub shortcuts: Vec<Shortcut>,
    pub scripts: Vec<Script>,
    pub data_sources: Vec<DataSource>,
    pub layer_templates: Vec<LayerTemplate>,
    /// Lets heavy files and network streams be decoded by the graphics card.
    pub hardware_decoding: bool,
}

impl Default for Project {
    fn default() -> Self {
        Self::blank()
    }
}

/// Takes the SRT passphrase out of an address.
pub fn without_passphrase(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else { return url.to_string() };
    let kept: Vec<&str> = query.split('&').filter(|p| !p.starts_with("passphrase=")).collect();
    if kept.is_empty() { head.to_string() } else { format!("{head}?{}", kept.join("&")) }
}

impl Project {
    /// A project file travels; keys and passphrases stay on this computer.
    pub fn without_secrets(mut self) -> Self {
        self.streams.iter_mut().for_each(|s| s.key.clear());
        self.srt.iter_mut().for_each(|o| o.url = without_passphrase(&o.url));
        for i in &mut self.inputs {
            match &mut i.kind {
                Kind::Network { url, .. } => *url = without_passphrase(url),
                Kind::VideoCall { token, turn_password, .. } => {
                    token.clear();
                    turn_password.clear();
                }
                _ => {}
            }
        }
        self
    }

    /// The empty desk a new production starts from.
    pub fn blank() -> Self {
        let colour = |color| Category { name: String::new(), color, enabled: true };
        let trans = |kind| TransSlot { kind, ms: 1000 };
        Self {
            inputs: Vec::new(),
            preview: None,
            program: None,
            overlays: [None; 8],
            overlay_last: [None; 8],
            overlay_cfg: [OverlayCfg::default(); 8],
            stingers: [StingerCfg::default(); 8],
            trans: [
                trans(TransKind::Fade),
                trans(TransKind::Merge),
                trans(TransKind::RollerDoor),
                trans(TransKind::Zoom),
            ],
            quick_play: TransSlot { kind: TransKind::Fade, ms: 500 },
            categories: vec![
                colour([204, 34, 34]),
                colour([30, 140, 60]),
                colour([226, 134, 26]),
                colour([130, 40, 170]),
                colour([24, 170, 190]),
                colour([34, 110, 214]),
            ],
            out_w: 1920,
            out_h: 1080,
            fps: 30,
            playlist: Playlist::default(),
            clocks: [ClockCfg::new(ClockType::TimeRemaining, 2), ClockCfg::new(ClockType::Clock, 0)],
            show_clocks: true,
            records: [RecordCfg::new(true, 25000), RecordCfg::new(false, 6000)],
            streams: Default::default(),
            srt: Default::default(),
            ndi_out: NetOutCfg::default(),
            omt_out: NetOutCfg::default(),
            master_level: 1.0,
            master_muted: false,
            buses: Default::default(),
            master_device: String::new(),
            master_plugins: Vec::new(),
            headphones_device: String::new(),
            headphones_level: 1.0,
            ftb_audio: true,
            fullscreen_route: [Route::Output, Route::Output],
            fullscreen_monitor: [2, 3],
            shortcuts: Vec::new(),
            scripts: Vec::new(),
            data_sources: Vec::new(),
            layer_templates: Vec::new(),
            hardware_decoding: true,
        }
    }

    /// A worked example: works with nothing installed, and shows the desk
    /// the way a real production fills it.
    pub fn sample() -> Self {
        let title = |text: &str, sub: &str, style, color, accent| Kind::Title {
            text: text.into(),
            sub: sub.into(),
            style,
            color,
            accent,
            ticker_speed: 4.0,
            countdown_ms: 300_000,
            font: String::new(),
            size: 0.0,
            presets: Vec::new(),
            data: None,
        };
        let full = |input| Layer::new(input, Placement::FULL);
        let corner = |input| Layer::new(input, Placement { x: 0.66, y: 0.64, scale: 0.3 });
        let left = |input| Layer::new(input, Placement { x: 0.0, y: 0.25, scale: 0.5 });
        let right = |input| Layer::new(input, Placement { x: 0.5, y: 0.25, scale: 0.5 });
        let white = [255, 255, 255];
        let orange = [240, 120, 40];
        let dark = [24, 28, 38];
        let lower = title(
            t!("model.sample.lower_title"),
            t!("model.sample.lower_subtitle"),
            TitleStyle::LowerThird,
            white,
            orange,
        );
        let ticker = title(t!("model.sample.ticker_text"), "", TitleStyle::Ticker, white, dark);
        let welcome = title(
            t!("model.sample.welcome_title"),
            t!("model.sample.welcome_subtitle"),
            TitleStyle::Centered,
            [244, 130, 44],
            dark,
        );
        let timer = title("", "", TitleStyle::Countdown, white, dark);
        let mut tone = Input::new(16, t!("model.kind.tone"), Kind::Tone).in_category(4);
        tone.audio.muted = true;
        let mut p = Self::blank();
        let categories = [
            t!("model.sample.cat_cameras"),
            t!("model.sample.cat_media"),
            t!("model.sample.cat_scenes"),
            t!("model.sample.cat_other"),
            t!("model.sample.cat_texts"),
            t!("model.sample.cat_tools"),
        ];
        for (n, name) in categories.iter().enumerate() {
            p.categories[n].name = (*name).to_string();
        }
        p.inputs = vec![
            Input::new(1, "CAM 1", Kind::Bars).in_category(0),
            Input::new(2, "CAM 2", Kind::Motion).in_category(0),
            Input::new(3, "CAM 3", Kind::Bars).in_category(0),
            Input::new(4, "CAM 4", Kind::Motion).in_category(0),
            Input::new(5, t!("model.sample.background"), Kind::Motion).in_category(1).folded(),
            Input::new(6, t!("model.sample.welcome"), welcome).in_category(4).folded(),
            Input::new(7, t!("model.sample.holding_screen"), Kind::Scene)
                .in_category(2)
                .with_layers(vec![full(5), full(6)]),
            Input::new(8, "1 CAM", Kind::Scene).in_category(2).with_layers(vec![full(1)]),
            Input::new(9, "2 CAM", Kind::Scene).in_category(2).with_layers(vec![left(1), right(2)]),
            Input::new(10, t!("model.sample.picture_in_picture"), Kind::Scene)
                .in_category(2)
                .with_layers(vec![full(2), corner(1)]),
            Input::new(11, t!("model.sample.lower_third"), lower).in_category(4),
            Input::new(12, t!("model.sample.ticker"), ticker).in_category(4),
            Input::new(13, t!("model.sample.countdown"), timer).in_category(4),
            Input::new(14, t!("model.kind.clock"), Kind::Clock).in_category(5),
            Input::new(15, t!("model.sample.blue_background"), Kind::Solid { color: [24, 62, 120] })
                .in_category(1),
            tone,
        ];
        p.preview = Some(8);
        p.program = Some(7);
        p
    }
}
