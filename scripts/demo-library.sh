#!/usr/bin/env bash
# Builds the synthetic demo library used by the headless screenshot tour
# (RICERCAR_SNAPSHOT) and for manual testing: 12 albums, 59 short FLAC tracks
# of sine tones at various rates/depths, generated covers, one .lrc file.
#
#   scripts/demo-library.sh <out-dir>
#
# Needs ffmpeg and ImageMagick 7 (`magick`). The album, artist and playlist
# names are referenced by crates/ricercar-ui/src/snapshot.rs: keep them in sync.
set -euo pipefail

D=${1:?usage: $0 <out-dir>}
for bin in ffmpeg magick; do
  command -v "$bin" >/dev/null || { echo "missing: $bin" >&2; exit 1; }
done
rm -rf "$D"
mkdir -p "$D"

plan=$(cat <<'EOF'
Aurelia Stone	Northern Lights	2019	Ambient	44100	16	Aurora|Polar Night|Glass Fjord|Magnetosphere|Tundra Hymn|First Light	0
Aurelia Stone	Salt & Silver	2022	Ambient	96000	24	Tidewater|Salt Flats|Silver Coast|Undertow|Low Tide	1
The Meridian Quartet	Blue Hours	1998	Jazz	96000	24	Blue Hour|Late Set|Brushes|Night Owl|Coda in F|Walking Home	2
The Meridian Quartet	Live at the Harbor	2003	Jazz	44100	16	Harbor Intro|Mooring|Fog Horn Blues|Last Ferry	3
Ensemble Lumen	Fugues & Ricercars	2016	Classical	192000	24	Ricercar a 3|Ricercar a 6|Canon per tonos|Fuga I|Fuga II|Contrapunctus	4
Ensemble Lumen	Winter Sonatas	2020	Classical	96000	24	Sonata I – Allegro|Sonata I – Adagio|Sonata II – Presto|Sonata III – Largo	5
Kaito Mori	Neon Rain	2021	Electronic	48000	24	Neon Rain|Shibuya 3AM|Circuit|Afterglow|Station Loop	6
Kaito Mori	Signal	2024	Electronic	44100	16	Carrier|Modulation|Noise Floor|Signal	7
Lena Vargas	Paper Houses	2017	Folk	44100	16	Paper Houses|Kitchen Light|Old Roads|Letters|Wild Thyme|Home Again	8
Lena Vargas	Evergreen	2023	Folk	88200	24	Evergreen|Rivers|Lantern|Hollow Oak	9
Orchestre Clair	Nocturnes	2011	Classical	44100	16	Nocturne No. 1|Nocturne No. 2|Nocturne No. 3|Nocturne No. 4	10
Soft Machines	Analog Dreams	1994	Electronic	44100	16	Analog Dreams|Tape Hiss|VHS Sunset|Reel to Reel|Rewind	11
EOF
)

n=0
while IFS=$'\t' read -r ar al y g sr b tr idx; do
  dir="$D/$ar/$al"
  mkdir -p "$dir"
  hue=$(( (idx * 37) % 360 )); hue2=$(( (hue + 140) % 360 ))
  magick -size 600x600 -seed $((idx + 7)) "plasma:hsl($hue,70%,45%)-hsl($hue2,60%,20%)" \
    -blur 0x6 -swirl $((idx * 40)) -fill "rgba(255,255,255,0.85)" -pointsize 44 \
    -gravity southwest -annotate +36+40 "$al" "$dir/cover.jpg" </dev/null
  IFS='|' read -ra titles <<< "$tr"
  t=1
  for title in "${titles[@]}"; do
    f=$(( 180 + (t * 47 + idx * 13) % 500 ))
    fmt=s16; [ "$b" = "24" ] && fmt=s32
    ffmpeg -nostdin -loglevel error \
      -f lavfi -i "sine=frequency=$f:sample_rate=$sr:duration=$((20 + (t * 7) % 25))" \
      -af "volume=0.2" -ac 2 -sample_fmt $fmt -bits_per_raw_sample "$b" \
      -metadata title="$title" -metadata artist="$ar" -metadata album_artist="$ar" \
      -metadata album="$al" -metadata date="$y" -metadata genre="$g" -metadata track="$t" \
      "$dir/$(printf %02d $t) $title.flac"
    t=$((t + 1)); n=$((n + 1))
  done
done <<< "$plan"

printf '[00:02.00]First line of a synthetic lyric\n[00:06.00]Second line, a little longer than the first\n[00:10.50]Third line\n[00:15.00]\n[00:17.00]The chorus comes back again\n[00:22.00]And fades out slowly\n' \
  > "$D/Kaito Mori/Neon Rain/01 Neon Rain.lrc"
echo "$n tracks in $D"
