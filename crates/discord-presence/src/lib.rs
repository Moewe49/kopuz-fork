pub mod cover_art;

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
use discord_rich_presence::{
    DiscordIpc, DiscordIpcClient,
    activity::{self, Assets, Timestamps},
};
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
use std::sync::Mutex;
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Self-healing Discord Rich Presence. The connection is established
/// lazily and re-established automatically:
///
/// - If Discord isn't running when the app starts, the first activity
///   update after Discord launches connects.
/// - If Discord closes or restarts mid-session, the broken pipe drops the
///   client and the next update reconnects.
///
/// (The old design connected exactly once in `new()` — start Kopuz before
/// Discord and presence stayed dead for the whole session.)
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
#[derive(Debug)]
pub struct Presence {
    client_id: String,
    client: Mutex<Option<DiscordIpcClient>>,
    last_attempt: Mutex<Option<Instant>>,
    /// When the pipe last carried a successful write. Drives [`Presence::drop_if_stale`].
    last_used: Mutex<Option<Instant>>,
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
const RECONNECT_THROTTLE: Duration = Duration::from_secs(5);

/// How long a connection may sit unused before it is thrown away rather than
/// trusted. See [`Presence::drop_if_stale`] for why an unused connection is the
/// dangerous kind.
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
const IDLE_RECONNECT: Duration = Duration::from_secs(60);

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
impl Presence {
    /// Never fails on desktop — the actual connect happens lazily on the
    /// first activity update (and is retried while Discord is closed).
    pub fn new(client_id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // Deliberately does NOT connect here.
        //
        // It used to, "so presence shows immediately when Discord is already
        // running" — which bought nothing (there is no activity to show until
        // something plays) and cost the whole feature. That connection then sat
        // unused from app start until the first song, and a connection nobody
        // writes to is exactly the one that dies quietly. See
        // [`Presence::drop_if_stale`].
        //
        // It also produced the tell-tale asymmetry: starting Discord AFTER
        // Kopuz worked, because then the connect happened right before the
        // first send. Starting Kopuz second showed nothing at all.
        Ok(Self {
            client_id: client_id.to_string(),
            client: Mutex::new(None),
            last_attempt: Mutex::new(None),
            last_used: Mutex::new(None),
        })
    }

    /// Throw away a connection that has been idle too long, so the next call
    /// builds a fresh one.
    ///
    /// Necessary because nothing here can tell a live pipe from a dead one:
    /// this IPC client never reads Discord's replies and never answers its
    /// PING frames, and on Windows the first write to a pipe whose far end has
    /// gone away is buffered and reports success. A dead connection therefore
    /// swallows updates while reporting `Ok`, which suppresses the retry that
    /// would have healed it — silence, permanently.
    ///
    /// Age is the only signal available, so age is what's used.
    fn drop_if_stale(&self) {
        let stale = {
            let last = self.last_used.lock().unwrap();
            last.is_none_or(|t| t.elapsed() > IDLE_RECONNECT)
        };
        if !stale {
            return;
        }
        let mut guard = self.client.lock().unwrap();
        if guard.is_none() {
            return;
        }
        if let Some(client) = guard.as_mut() {
            let _ = client.close();
        }
        *guard = None;
        // Reconnect now rather than sitting out the throttle: this is a
        // deliberate refresh, not a failed attempt.
        *self.last_attempt.lock().unwrap() = None;
    }

    /// (Re)connect if needed. Attempts are throttled so a closed Discord
    /// doesn't get hammered every player tick.
    fn ensure_connected(&self) -> bool {
        let mut guard = self.client.lock().unwrap();
        if guard.is_some() {
            return true;
        }
        {
            let mut last = self.last_attempt.lock().unwrap();
            if let Some(t) = *last
                && t.elapsed() < RECONNECT_THROTTLE
            {
                return false;
            }
            *last = Some(Instant::now());
        }
        let mut client = DiscordIpcClient::new(&self.client_id);
        match client.connect() {
            Ok(()) => {
                eprintln!("[discord] connected (app id {})", self.client_id);
                *guard = Some(client);
                true
            }
            Err(e) => {
                eprintln!("[discord] connect failed (is Discord running?): {e}");
                false
            }
        }
    }

    /// Run `f` against a live client; on error assume the pipe died
    /// (Discord closed/restarted), drop the client so the next call
    /// reconnects, and surface the error.
    fn with_client(
        &self,
        f: impl FnOnce(&mut DiscordIpcClient) -> Result<(), Box<dyn std::error::Error>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.drop_if_stale();
        if !self.ensure_connected() {
            return Err("Discord is not running".into());
        }
        let mut guard = self.client.lock().unwrap();
        let Some(client) = guard.as_mut() else {
            return Err("Discord is not running".into());
        };
        match f(client) {
            Ok(()) => {
                *self.last_used.lock().unwrap() = Some(Instant::now());
                Ok(())
            }
            Err(e) => {
                eprintln!("[discord] set_activity failed (pipe likely closed): {e}");
                let _ = client.close();
                *guard = None;
                Err(e)
            }
        }
    }

    pub fn disconnect(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(mut client) = self.client.lock().unwrap().take() {
            client.close()?;
        }
        Ok(())
    }

    /// `m:ss`, or `h:mm:ss` past the hour — the shape a player shows, so a
    /// paused position reads as a position and not as a raw number.
    fn format_clock(total_secs: u64) -> String {
        let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }

    pub fn set_now_playing(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        elapsed_secs: u64,
        duration_secs: u64,
        cover_url: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        let start_time = now - elapsed_secs as i64;
        let end_time = start_time + duration_secs as i64;

        let timestamps = if duration_secs == u64::MAX {
            Timestamps::new().start(start_time)
        } else {
            Timestamps::new().start(start_time).end(end_time)
        };

        let state = format!("{artist}");

        let mut activity = activity::Activity::new()
            .details(title)
            .state(&state)
            .status_display_type(activity::StatusDisplayType::State)
            .timestamps(timestamps)
            .activity_type(activity::ActivityType::Listening);

        if let Some(url) = cover_url {
            // Cover only — no album text. Discord shows just the song (details)
            // and the artist (state); `album` stays in the signature because the
            // cover lookup still keys on it, but it is never displayed.
            let _ = album;
            let assets = Assets::new().large_image(url);
            activity = activity.assets(assets);
        }

        self.with_client(|c| Ok(c.set_activity(activity)?))
    }

    pub fn set_paused(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        elapsed_secs: u64,
        duration_secs: u64,
        cover_url: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Discord has NO paused state and NO way to freeze a progress bar: a bar
        // is always derived from a `start` timestamp and animated live against
        // the wall clock, so ANY timestamp keeps moving. The old approach
        // backdated `start` and re-sent every few seconds to snap it back — but
        // between snaps the bar visibly creeps, so a paused track just looked
        // like it was still playing ("der Balken läuft weiter").
        //
        // So when paused we send **no timestamps at all** — then Discord draws no
        // bar and nothing animates. The frozen position lives in the TEXT (the
        // one part of an activity that genuinely holds still), shown as
        // `⏸ m:ss / m:ss`, which reads unambiguously as paused-at-a-position.
        //
        // (The caller still re-sends this every few seconds. That's not to move a
        // bar — there's none now — but to keep the IPC pipe warm so an idle
        // connection isn't dropped mid-pause, which is what made the activity
        // vanish entirely before.)
        let position = if duration_secs == 0 || duration_secs == u64::MAX {
            Self::format_clock(elapsed_secs)
        } else {
            format!(
                "{} / {}",
                Self::format_clock(elapsed_secs),
                Self::format_clock(duration_secs)
            )
        };
        let state = format!("{artist} • ⏸ {position}");
        // CRUCIAL: send an EXPLICIT empty timestamps object, not an omitted one.
        // Omitting the key means "unchanged" to Discord, so it keeps the `start`
        // from the last *playing* update and shows a green timer that keeps
        // climbing (2:08, 2:09, …) even though we're paused. An explicit empty
        // `{}` clears it — no bar, no counter — leaving just the frozen position
        // in the text.
        let mut activity = activity::Activity::new()
            .details(title)
            .state(&state)
            .status_display_type(activity::StatusDisplayType::State)
            .timestamps(Timestamps::new())
            .activity_type(activity::ActivityType::Listening);

        if let Some(url) = cover_url {
            // Cover only — no album text. Discord shows just the song (details)
            // and the artist (state); `album` stays in the signature because the
            // cover lookup still keys on it, but it is never displayed.
            let _ = album;
            let assets = Assets::new().large_image(url);
            activity = activity.assets(assets);
        }

        self.with_client(|c| Ok(c.set_activity(activity)?))
    }

    /// Diagnostic: connect (if needed) and push a clearly-labelled test
    /// activity. Returns a human-readable status the Settings UI shows, so the
    /// user can tell apart "Discord isn't running", "connected but the activity
    /// was rejected", and "connected — check your Discord profile". The most
    /// common silent cause is Discord's *User Settings → Activity Privacy →
    /// Display current activity* being off, which this can't detect from here.
    pub fn test(&self) -> Result<String, String> {
        // Force a real (non-throttled) reconnect attempt for the test.
        *self.last_attempt.lock().unwrap() = None;
        if !self.ensure_connected() {
            return Err(
                "Couldn't reach Discord. Make sure the Discord desktop app is running \
                 (the browser version can't show Rich Presence)."
                    .to_string(),
            );
        }
        match self.set_now_playing(
            "Kopuz — presence test",
            "If you can see this on your Discord profile, it works",
            "",
            0,
            180,
            None,
        ) {
            Ok(()) => Ok(
                "Connected. Check your Discord profile — you should see \"Kopuz — presence test\". \
                 If not, enable Discord → Settings → Activity Privacy → \"Display current activity\"."
                    .to_string(),
            ),
            Err(e) => Err(format!("Connected, but Discord rejected the activity: {e}")),
        }
    }

    /// True if we currently hold a live connection (does not attempt to connect).
    pub fn is_connected(&self) -> bool {
        self.client.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    pub fn clear_activity(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Nothing to clear if we're not even connected — and we must NOT
        // trigger a reconnect attempt just to clear.
        let mut guard = self.client.lock().unwrap();
        let Some(client) = guard.as_mut() else {
            return Ok(());
        };
        if let Err(e) = client.clear_activity() {
            let _ = client.close();
            *guard = None;
            return Err(Box::new(e));
        }
        Ok(())
    }
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
impl Drop for Presence {
    fn drop(&mut self) {
        let mut guard = match self.client.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(client) = guard.as_mut() {
            let _ = client.close();
        }
    }
}

// Android has no Discord IPC; this no-op stub keeps the `Presence` API surface so the
// shared player-task code compiles unchanged. The app never constructs it on Android
// (`Presence::new` errors), so the context stays `None` and every call site is skipped.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct Presence;

#[cfg(target_os = "android")]
impl Presence {
    pub fn new(_client_id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Err("Discord presence is not available on Android".into())
    }

    pub fn disconnect(&self) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    pub fn set_now_playing(
        &self,
        _title: &str,
        _artist: &str,
        _album: &str,
        _elapsed_secs: u64,
        _duration_secs: u64,
        _cover_url: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    pub fn set_paused(
        &self,
        _title: &str,
        _artist: &str,
        _album: &str,
        _elapsed_secs: u64,
        _duration_secs: u64,
        _cover_url: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    pub fn clear_activity(&self) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    pub fn test(&self) -> Result<String, String> {
        Err("Discord presence is not available on this platform".to_string())
    }

    pub fn is_connected(&self) -> bool {
        false
    }
}

#[cfg(all(test, not(target_arch = "wasm32"), not(target_os = "android")))]
mod tests {
    use super::*;

    /// The paused position is the one number Discord will hold still, so it has
    /// to read as a position rather than as a raw second count.
    #[test]
    fn a_paused_position_is_formatted_the_way_a_player_shows_it() {
        assert_eq!(Presence::format_clock(0), "0:00");
        assert_eq!(Presence::format_clock(9), "0:09");
        assert_eq!(Presence::format_clock(184), "3:04");
        // Seconds must stay zero-padded past the minute boundary — "3:4" would
        // read as three minutes four seconds to nobody.
        assert_eq!(Presence::format_clock(3 * 60 + 4), "3:04");
        // Long mixes and audiobook chapters run past an hour.
        assert_eq!(Presence::format_clock(3600), "1:00:00");
        assert_eq!(Presence::format_clock(3661), "1:01:01");
        assert_eq!(Presence::format_clock(7384), "2:03:04");
    }
}
