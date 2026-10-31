use crate::config::TabPos;
use crate::emote::fb::FbEmotes;
use crate::emote::render::EmoteStore;
use crate::net::ChatEvent;
use crate::paints::Paints;
use crate::{hsauth, kick, net, picker, twitch};
use heatsync_core::complete::Completion;
use heatsync_core::edit;
use heatsync_core::emote::EmoteSet;
use heatsync_core::room::RoomModes;
use heatsync_core::streamev::StreamEvents;
use heatsync_core::{mock, Channel, Message, Platform};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// feed source: offline synthetic, or the live relay thread.
pub(crate) enum Feed {
    Mock(mock::Driver),
    Live {
        rx: Receiver<ChatEvent>,
        start: Instant,
        connected: bool,
    },
}

/// modes: switch channels in Normal, type a message in Insert, pick a live channel
/// in Pick, or manage channels rover-style in Manage. SlotPick/SlotEdit
/// are Manage sub-modes: pick one of the t/k/y source slots, then retype it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InputMode {
    Normal,
    Insert,
    Manage,
    /// e pressed in Manage — waiting for t/k/y to choose which slot to edit.
    SlotPick,
    /// typing a new name for one platform slot of the row under the cursor.
    SlotEdit(Platform),
    /// the live-channel picker (`o`).
    Pick,
    /// the message cursor (`v`): move through scrollback, reply, yank, delete.
    Cursor,
}

/// a fetched pin for (platform, channel); `None` = the channel has none.
pub(crate) type PinMsg = (Platform, String, Option<heatsync_core::Pin>);

pub(crate) struct App {
    pub(crate) channels: Vec<Channel>,
    pub(crate) emotes: Vec<EmoteSet>, // index-aligned with channels
    /// the pooled global set (twitch + unlocked + bttv/ffz/7tv globals),
    /// merged under every channel set — channel emotes of the same name win.
    pub(crate) globals: EmoteSet,
    pub(crate) focus: usize,
    pub(crate) paused: bool,
    pub(crate) feed: Feed,
    pub(crate) store: Option<EmoteStore>, // terminal graphics tier (sixel/kitty/…)
    pub(crate) fb: Option<FbEmotes>,      // bare-console framebuffer tier (TERM=linux)
    pub(crate) mode: InputMode,
    pub(crate) input: String,
    /// the composer: a vi line editor with its own insert/normal modes.
    pub(crate) line: edit::Line,
    pub(crate) status: Option<String>, // transient one-line notice (send errors, etc.)
    pub(crate) out: Option<net::Tx>,   // outbound channel to the live WS thread
    pub(crate) twitch_tx: Option<std::sync::mpsc::Sender<twitch::Send>>, // direct twitch sender
    // what the sender has to say back: NOTICE rejections, auth failure.
    pub(crate) twitch_notes: Option<Receiver<twitch::Note>>,
    pub(crate) kick_tx: Option<std::sync::mpsc::Sender<kick::Send>>, // direct kick sender
    /// the heatsync login: sends and mod actions go through heatsync.org with it.
    pub(crate) hs: Option<hsauth::Client>,
    /// finished sends / mod actions come back here, from their own threads.
    pub(crate) hs_tx: Sender<hsauth::Note>,
    pub(crate) hs_notes: Receiver<hsauth::Note>,
    pub(crate) tab_pos: TabPos,
    pub(crate) manage_cursor: usize, // cursor in the Manage view
    /// a tab-completion walk in progress; any non-tab key ends it.
    pub(crate) completion: Option<Completion>,
    /// my names (twitch login, kick login) for mention + reply-to-me pings.
    pub(crate) me: Vec<String>,
    /// the heatsync login's names arrive here once, from a startup thread.
    pub(crate) names_rx: Receiver<Vec<String>>,
    /// dim `HH:MM` before each line (`t`, persisted).
    pub(crate) stamps: bool,
    // emote sets are fetched off-thread (a blocking HTTP call would freeze the
    // UI); results arrive here keyed by (platform, name) and merge in.
    pub(crate) emote_tx: Sender<(Platform, String, EmoteSet)>,
    pub(crate) emote_rx: Receiver<(Platform, String, EmoteSet)>,
    // archived scrollback arrives the same way: fetched per channel at join,
    // merged in behind whatever live lines have already landed.
    pub(crate) hist_tx: Sender<(Platform, String, Vec<Message>)>,
    pub(crate) hist_rx: Receiver<(Platform, String, Vec<Message>)>,
    /// the channel picker: its state while open, the last fetch (reused for
    /// 30s), and the thread's way back.
    pub(crate) picker: Option<picker::Picker>,
    pub(crate) pick_cache: picker::Cache,
    pub(crate) pick_tx: Sender<picker::Fetched>,
    pub(crate) pick_rx: Receiver<picker::Fetched>,
    pub(crate) pick_inflight: bool,
    /// name paints + heatsync name colors, by sender id.
    pub(crate) paints: Paints,
    /// pins fetched on join arrive here from their own threads.
    pub(crate) pin_tx: Sender<PinMsg>,
    pub(crate) pin_rx: Receiver<PinMsg>,
    /// the message cursor's line, as an absolute sequence number (core::scroll).
    pub(crate) cursor: u64,
    /// the line being replied to: (message id, user). set by `r` in cursor mode.
    pub(crate) reply: Option<(String, String)>,
    /// rows the chat pane had at the last draw — half a page for ctrl-d/u.
    pub(crate) view_h: std::cell::Cell<u16>,
    /// where the chat body sat in the last drawn frame (empty when none was drawn).
    pub(crate) chat_body: std::cell::Cell<ratatui::layout::Rect>,
    /// stream-event pairing + anti-wall guards, between the feed and the tabs.
    pub(crate) streams: StreamEvents,
    /// last-seen room modes per channel, for `slow mode 30s`-style notes.
    pub(crate) rooms: RoomModes,
    /// mention log + feed backlog channel for the virtual tabs.
    pub(crate) special: crate::special::State,
}
