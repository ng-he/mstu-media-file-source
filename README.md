# mstu-media-file-source

An [mstu](https://github.com/ng-he/mstu) source plugin: reads a media file and
emits its video samples, paced to the timeline or as fast as the pipeline will
take them.

## What it reads

Any file in the ISO base media format, which is recognised by its first box
rather than by its name: `.mp4`, `.mov`, `.m4v`, `.3gp` and the rest of the
family, including files whose extension says nothing or says the wrong thing.

| Sample entry | Emitted as | On the way out |
| --- | --- | --- |
| `avc1`, `avc3` | `h264` | AVCC rewritten to Annex-B, parameter sets first |
| `hev1`, `hvc1` | `h265` | AVCC rewritten to Annex-B, parameter sets first |
| `av01` | `av1` | a temporal delimiter per sample, which the file leaves out |
| `vp09` | `vp9` | frames as the file stores them |

The first video track is the one that plays. Audio and subtitle tracks are
parsed but not emitted.

Raises `file.changed` when a file is opened, carrying the codec and whatever a
decoder needs in front of the stream, and `file.ended` at the end of it.

```sh
./build.sh
```

The library lands in `target/release`; the center repo's `collect.sh` installs
it, along with `ui/`, where the engine looks.
