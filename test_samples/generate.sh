#!/usr/bin/env bash
# Writes the sample files the tests read, and prints what is in each one.
#
# The files are committed, so this is only needed to add one or to make them
# again with a different encoder. Run it from anywhere:
#
#     ./test_samples/generate.sh
#
# Afterwards, put the counts it prints into the table in the tests, which is
# what they are checked against.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

# Two seconds of test pattern, small enough to keep in the repository.
SOURCE=(-f lavfi -i "testsrc=size=320x240:rate=30:duration=2")

make() {
    local name="$1"
    shift

    echo "==> $name"
    ffmpeg -hide_banner -loglevel error -y "${SOURCE[@]}" "$@" "$name"
}

make h264.mp4 -c:v libx264
make h264.mov -c:v libx264
make hvc1.mp4 -c:v libx265 -tag:v hvc1
make hev1.mp4 -c:v libx265
make av1.mp4 -c:v libsvtav1
make vp9.mp4 -c:v libvpx-vp9

# Frames shown in a different order than they are stored. x264 reorders by
# default, so the one that does not is the one worth asking for.
make bframes.mp4 -c:v libx264 -bf 3
make inorder.mp4 -c:v libx264 -bf 0

# Several groups of pictures, so the sync table has more than one entry.
make longgop.mp4 -c:v libx264 -g 20

# Written in pieces, which this reader does not read yet.
make fragmented.mp4 -c:v libx264 -movflags frag_keyframe+empty_moov

# Sound beside the picture, so the reader has to pick a track.
echo "==> with-sound.mp4"
ffmpeg -hide_banner -loglevel error -y \
    "${SOURCE[@]}" \
    -f lavfi -i "sine=frequency=440:duration=2" \
    -c:v libx264 -c:a aac -shortest \
    with-sound.mp4

echo
echo "what each one holds, for the table in the tests:"
printf "%-18s %-6s %7s %7s\n" file codec frames keys

for file in *.mp4 *.mov; do
    codec=$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name -of csv=p=0 "$file")
    frames=$(ffprobe -v error -select_streams v:0 -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$file")
    keys=$(ffprobe -v error -select_streams v:0 -show_entries frame=key_frame -of csv=p=0 "$file" |
        tr -d ',' | grep -c '^1$' || true)

    printf "%-18s %-6s %7s %7s\n" "$file" "$codec" "${frames:-0}" "$keys"
done
