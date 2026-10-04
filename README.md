# media-file-source

An [mstu](https://github.com/ng-he/mstu) source plugin: reads an MP4 file and
emits its video samples, paced to the timeline or as fast as the pipeline will
take them. H.264 and H.265, rewritten to Annex-B on the way out.

Raises `file.changed` when a file is opened, carrying the codec and its
parameter sets, and `file.ended` at the end of the stream.

```sh
./build.sh
```

The library lands in `target/release`; the center repo's `collect.sh` installs
it, along with `ui/`, where the engine looks.
