//! Strings produced on the Rust side (toasts, statuses, chain labels) and
//! small formatting helpers. The .slint side is translated through gettext.

use std::sync::OnceLock;

static FRENCH: OnceLock<bool> = OnceLock::new();

/// UI language from the usual POSIX variables (LC_ALL > LC_MESSAGES > LANG).
pub fn detect_language() -> &'static str {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let fr = lang.starts_with("fr");
    let _ = FRENCH.set(fr);
    if fr { "fr" } else { "en" }
}

fn fr() -> bool {
    *FRENCH.get().unwrap_or(&false)
}

/// Translate a Rust-side string (English is the key).
pub fn t(en: &'static str) -> &'static str {
    if !fr() {
        return en;
    }
    match en {
        "Good morning" => "Bonjour",
        "Hide window" => "Masquer la fenêtre",
        "Decoded to" => "Décodé en",
        "Muted" => "Sourdine",
        "Software volume" => "Volume logiciel",
        "ReplayGain / preamp" => "ReplayGain / préampli",
        "exclusive, no mixer" => "exclusif, sans mixeur",
        "shared: may resample or mix" => "partagé : peut rééchantillonner ou mixer",
        "Null sink: audio discarded" => "Sortie nulle : audio ignoré",
        "written to a file" => "écrit dans un fichier",
        "Null sink" => "Sortie nulle",
        "Discards audio (testing)" => "Ignore l'audio (tests)",
        "Show window" => "Afficher la fenêtre",
        "Pause" => "Pause",
        "Play" => "Lecture",
        "Next" => "Suivant",
        "Previous" => "Précédent",
        "Quit" => "Quitter",
        "Good afternoon" => "Bon après-midi",
        "Good evening" => "Bonsoir",
        "Good night" => "Bonne nuit",
        "Added to the queue" => "Ajouté à la file d'attente",
        "Will play next" => "Sera lu ensuite",
        "Removed from the playlist" => "Retiré de la playlist",
        "Added to favorites" => "Ajouté aux favoris",
        "Removed from favorites" => "Retiré des favoris",
        "Playlist created" => "Playlist créée",
        "Playlist deleted" => "Playlist supprimée",
        "Playlist exported to" => "Playlist exportée dans",
        "Playlist imported" => "Playlist importée",
        "Could not open the file" => "Impossible d'ouvrir le fichier",
        "Source" => "Source",
        "Decoder" => "Décodeur",
        "Output format" => "Format de sortie",
        "Device" => "Périphérique",
        "Volume" => "Volume",
        "Processing" => "Traitement",
        "none" => "aucun",
        "Searching for lyrics…" => "Recherche des paroles…",
        "No lyrics for this track" => "Pas de paroles pour ce titre",
        "Instrumental" => "Instrumental",
        "Online lyrics are turned off in Settings" => {
            "Les paroles en ligne sont désactivées dans les Réglages"
        }
        "Nothing is playing" => "Aucune lecture en cours",
        "Lyrics from lrclib.net" => "Paroles : lrclib.net",
        "Lyrics from the file" => "Paroles : fichier",
        "Lyrics from the .lrc file" => "Paroles : fichier .lrc",
        "Top stations" => "Stations populaires",
        "Stations" => "Stations",
        "Radio Browser is unreachable" => "Radio Browser est injoignable",
        "Searching…" => "Recherche…",
        "No station found" => "Aucune station trouvée",
        "Disc" => "Disque",
        "Playing from" => "Lecture depuis",
        "UPnP" => "UPnP",
        "Internet radio" => "Radio en ligne",
        "Library mix" => "Mix de la bibliothèque",
        "Not connected" => "Non connecté",
        "Connected as" => "Connecté en tant que",
        "Checking…" => "Vérification…",
        "Invalid token" => "Jeton invalide",
        "Approve ricercar in your browser, then click “I approved it”." => {
            "Autorisez ricercar dans votre navigateur, puis cliquez sur « J'ai autorisé l'accès »."
        }
        "Authorization failed" => "Échec de l'autorisation",
        "Visible on the network as" => "Visible sur le réseau sous le nom",
        "port" => "port",
        "Renderer is off" => "Le renderer est désactivé",
        "Folder added; indexing…" => "Dossier ajouté ; indexation…",
        "Folder removed" => "Dossier retiré",
        "Output device" => "Sortie audio",
        "Network error" => "Erreur réseau",
        "Unknown artist" => "Artiste inconnu",
        "tracks" => "titres",
        "track" => "titre",
        "Enter the API key and shared secret of your Last.fm API account first." => {
            "Saisissez d'abord la clé API et le secret partagé de votre compte API Last.fm."
        }
        _ => en,
    }
}

pub fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "1 h 12 min", "48 min", "312 h".
pub fn long_duration(ms: u64) -> String {
    let mins = (ms + 30_000) / 60_000;
    match mins {
        0..=59 => format!("{mins} min"),
        60..=5999 if !mins.is_multiple_of(60) && mins < 600 => {
            format!("{} h {} min", mins / 60, mins % 60)
        }
        _ => format!("{} h", mins / 60),
    }
}

/// "24/96", "16/44.1", "320k" (lossy).
pub fn quality(
    rate: Option<u32>,
    bits: Option<u8>,
    codec: Option<&str>,
    bitrate: Option<u32>,
) -> String {
    let lossy = matches!(codec, Some("MP3" | "AAC" | "Vorbis" | "Opus"));
    if lossy {
        return bitrate.map(|b| format!("{b}k")).unwrap_or_default();
    }
    match (bits, rate) {
        (Some(b), Some(r)) => format!("{b}/{}", khz(r)),
        (None, Some(r)) => khz(r),
        _ => String::new(),
    }
}

pub fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}", rate / 1000)
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

pub fn greeting() -> &'static str {
    use chrono::Timelike;
    match chrono::Local::now().hour() {
        5..=11 => t("Good morning"),
        12..=17 => t("Good afternoon"),
        18..=22 => t("Good evening"),
        _ => t("Good night"),
    }
}

pub fn count(n: usize, one: &'static str, many: &'static str) -> String {
    format!("{n} {}", if n == 1 { t(one) } else { t(many) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(mmss(61_000), "1:01");
        assert_eq!(mmss(3_725_000), "1:02:05");
        assert_eq!(long_duration(47 * 60_000), "47 min");
        assert_eq!(long_duration(72 * 60_000), "1 h 12 min");
        assert_eq!(long_duration(3000 * 60_000), "50 h");
        assert_eq!(quality(Some(96_000), Some(24), Some("FLAC"), None), "24/96");
        assert_eq!(
            quality(Some(44_100), Some(16), Some("FLAC"), None),
            "16/44.1"
        );
        assert_eq!(quality(Some(44_100), None, Some("MP3"), Some(320)), "320k");
    }
}
