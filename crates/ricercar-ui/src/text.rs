//! Strings produced on the Rust side (toasts, statuses, chain labels) and
//! small formatting helpers. The .slint side is translated through gettext.

use std::sync::atomic::{AtomicBool, Ordering};

static FRENCH: AtomicBool = AtomicBool::new(false);

/// Pick the interface language: `pref` ("en", "fr") from the settings, or
/// when empty the usual POSIX variables (LC_ALL > LC_MESSAGES > LANG).
/// Applies it to both the Rust strings and the .slint side.
pub fn set_language(pref: &str) -> &'static str {
    let lang = if pref.is_empty() {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    } else {
        pref.to_string()
    };
    let fr = lang.starts_with("fr");
    FRENCH.store(fr, Ordering::Relaxed);
    let code = if fr { "fr" } else { "en" };
    let _ = slint::select_bundled_translation(code);
    code
}

fn fr() -> bool {
    FRENCH.load(Ordering::Relaxed)
}

/// Language in use ("en" or "fr").
pub fn language() -> &'static str {
    if fr() { "fr" } else { "en" }
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
        "Network sharing is off" => "Le partage réseau est désactivé",
        "Starting…" => "Démarrage…",
        "Network unavailable" => "Réseau indisponible",
        "Reading capabilities…" => "Lecture des capacités…",
        "In use: capabilities from the last check." => {
            "En cours d'utilisation : capacités du dernier relevé."
        }
        "In use: capabilities are read once playback stops." => {
            "En cours d'utilisation : capacités relevées à l'arrêt de la lecture."
        }
        "Could not read the device" => "Impossible d'interroger le périphérique",
        "Diagnostic report copied" => "Rapport de diagnostic copié",
        "Sign in to" => "Se connecter à",
        "Signed in to" => "Connecté à",
        "Not found" => "Introuvable",
        "Not available (region, subscription or format)" => {
            "Non disponible (région, abonnement ou format)"
        }
        "Too many requests, try again later" => "Trop de requêtes, réessayez plus tard",
        "Offline: the service could not be reached" => "Hors ligne : le service est injoignable",
        "not running" => "ne tourne pas",
        "no answer" => "pas de réponse",
        "Off" => "Désactivé",
        "Restarting in" => "Redémarrage dans",
        "Stopped" => "Arrêté",
        "Ready" => "Prêt",
        "Signed in" => "Connecté",
        "Signed in as" => "Connecté en tant que",
        "Sign-in expired" => "Connexion expirée",
        "Signed out" => "Déconnecté",
        "Library" => "Bibliothèque",
        "All" => "Tout",
        "Everything" => "Partout",
        "My library" => "Ma bibliothèque",
        "Playlist" => "Playlist",
        "The plugin gave an unusable address." => "Le plugin a fourni une adresse inutilisable.",
        "Not signed in yet. Check the code and try again." => {
            "Pas encore connecté. Vérifiez le code et réessayez."
        }
        "Nothing playable here" => "Rien de lisible ici",
        "This plugin has no favourites" => "Ce plugin ne gère pas les favoris",
        "No results" => "Aucun résultat",
        "plugin" => "plugin",
        "Stream" => "Flux",
        "The community catalogue is off (Settings → Online extras)." => {
            "Le catalogue communautaire est désactivé (Réglages → Extras en ligne)."
        }
        "Loading the catalogue…" => "Chargement du catalogue…",
        "The catalogue is empty for now." => "Le catalogue est vide pour l'instant.",
        "Could not read the catalogue" => "Impossible de lire le catalogue",
        "sign-in" => "connexion",
        "browse" => "navigation",
        "search" => "recherche",
        "favourites" => "favoris",
        "remote control" => "contrôle à distance",
        "by" => "par",
        "Update" => "Mettre à jour",
        "Install" => "Installer",
        "Could not reach GitHub. Check the connection and try again." => {
            "Impossible de joindre GitHub. Vérifiez la connexion et réessayez."
        }
        "Update cancelled." => "Mise à jour annulée.",
        "The download does not match the release checksums; nothing was installed." => {
            "Le téléchargement ne correspond pas aux sommes de contrôle de la version ; rien n'a été installé."
        }
        "The download failed. Check the connection and try again." => {
            "Le téléchargement a échoué. Vérifiez la connexion et réessayez."
        }
        "The update could not be installed." => "La mise à jour n'a pas pu être installée.",
        "Installed" => "Installé",
        "Removed" => "Retiré",
        "this computer" => "cet ordinateur",
        "Report saved to" => "Rapport enregistré dans",
        "Folder added; indexing…" => "Dossier ajouté ; indexation…",
        "This folder does not exist." => "Ce dossier n'existe pas.",
        "Not authorised. Is a polkit authentication agent running?" => {
            "Non autorisé. Un agent d'authentification polkit est-il lancé ?"
        }
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

/// kHz with the decimal separator of the language ("352,8" in French).
fn khz_local(rate: u32) -> String {
    let k = khz(rate);
    if fr() { k.replace('.', ",") } else { k }
}

/// "12 albums at 352.8 kHz can't be played natively on this DAC".
pub fn unsupported_albums(n: u32, rate: u32) -> String {
    let k = khz_local(rate);
    match (fr(), n == 1) {
        (false, true) => format!("1 album at {k} kHz can't be played natively on this DAC"),
        (false, false) => format!("{n} albums at {k} kHz can't be played natively on this DAC"),
        (true, true) => format!("1 album à {k} kHz ne pourra pas être lu nativement sur ce DAC"),
        (true, false) => {
            format!("{n} albums à {k} kHz ne pourront pas être lus nativement sur ce DAC")
        }
    }
}

/// Install confirmation: what comes from where, and what is not checked.
pub fn install_body(name: &str, version: &str, author: &str, host: &str, repo: &str) -> String {
    let who = if author.is_empty() {
        String::new()
    } else if fr() {
        format!(" de {author}")
    } else {
        format!(" by {author}")
    };
    if fr() {
        format!(
            "{name} {version}{who} sera téléchargé depuis {host} et lancé avec vos droits. ricercar vérifie que le fichier correspond au catalogue (SHA-256), mais ne relit pas ce que fait le plugin. Code source : {repo}"
        )
    } else {
        format!(
            "{name} {version}{who} will be downloaded from {host} and run with your permissions. ricercar checks that the file matches the catalogue (SHA-256) but does not review what the plugin does. Source: {repo}"
        )
    }
}

/// Warning shown above the confirmation of a plugin update whose binary now
/// comes from another host than the installed one.
pub fn host_changed(old: &str, new: &str) -> String {
    if fr() {
        format!(
            "Attention : l'adresse de téléchargement a changé. La version installée venait de {old}, cette mise à jour vient de {new}. Ne continuez que si vous faites confiance à cette nouvelle source."
        )
    } else {
        format!(
            "Warning: the download address has changed. The installed version came from {old}; this update comes from {new}. Only continue if you trust the new source."
        )
    }
}

/// "The DAC refuses 352.8 kHz (accepts: 44.1–192 kHz)".
pub fn dac_refuses(rate: u32, accepts: Option<&str>) -> String {
    let k = khz_local(rate);
    let accepts = accepts.map(|a| {
        if fr() {
            a.replace('.', ",")
        } else {
            a.to_string()
        }
    });
    match (fr(), accepts) {
        (false, Some(a)) => format!("The DAC refuses {k} kHz (accepts: {a})"),
        (false, None) => format!("The DAC refuses {k} kHz"),
        (true, Some(a)) => format!("Le DAC refuse {k} kHz (accepte : {a})"),
        (true, None) => format!("Le DAC refuse {k} kHz"),
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
        assert_eq!(
            unsupported_albums(12, 352_800),
            "12 albums at 352.8 kHz can't be played natively on this DAC"
        );
        assert_eq!(
            dac_refuses(352_800, Some("44.1–192 kHz")),
            "The DAC refuses 352.8 kHz (accepts: 44.1–192 kHz)"
        );
    }
}
