//! Lyrics of tracks from source plugins: the plugin first (capability
//! `lyrics`), then LRCLIB. Glue between the plugin host and
//! `ricercar-online`, which knows nothing of plugins.

use std::path::Path;

use ricercar_core::TrackInfo;
use ricercar_core::plugin::{PluginError, PluginHost, parse_plugin_uri};
use ricercar_online::lyrics::{
    LrclibQuery, LyricLine, Lyrics, LyricsCache, LyricsSource, PluginAnswer, fetch_for_plugin_track,
};

/// Lyrics of a `plugin://` track, `None` when nobody has any. Blocking
/// (plugin call, then network): call it off the UI thread. `lrclib`: the
/// user allows online lyrics (`[online] lyrics`). `cache_dir` is the lyrics
/// cache (`<cache>/lyrics`). The answer's `source` is
/// `LyricsSource::Plugin(id)` when the plugin gave them.
pub fn for_plugin_track(
    host: &PluginHost,
    cache_dir: &Path,
    info: &TrackInfo,
    lrclib: bool,
) -> ricercar_online::Result<Option<Lyrics>> {
    let Some((id, reference)) = parse_plugin_uri(&info.uri) else {
        return Ok(None);
    };
    let cache = LyricsCache::new(cache_dir);
    let declares = host.status(&id).is_some_and(|s| s.caps.lyrics);
    let mut ask = || match host.lyrics_get(&id, &reference) {
        Ok(l) => PluginAnswer::Found(Lyrics {
            synced: l.synced.map(|s| {
                s.into_iter()
                    .map(|x| LyricLine {
                        time_ms: x.time_ms,
                        text: x.text,
                    })
                    .collect()
            }),
            plain: l.plain,
            source: LyricsSource::Plugin(id.clone()),
            instrumental: l.instrumental,
        }),
        Err(PluginError::NotFound) => PluginAnswer::NotFound,
        Err(e) => {
            tracing::debug!("plugin[{id}] lyrics.get: {e}");
            PluginAnswer::Failed
        }
    };
    let query = info
        .artist
        .clone()
        .filter(|a| lrclib && !a.trim().is_empty())
        .map(|artist| LrclibQuery {
            artist,
            title: info.title.clone(),
            album: info.album.clone(),
            duration_s: (info.duration_ms > 0).then_some((info.duration_ms / 1000) as u32),
        });
    fetch_for_plugin_track(
        &cache,
        &info.uri,
        declares.then_some(&mut ask as &mut dyn FnMut() -> PluginAnswer),
        query.as_ref(),
    )
}
