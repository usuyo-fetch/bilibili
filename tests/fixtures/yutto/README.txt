NOT MEASURED. Every file in this folder is hand-written with invented content. None of it was produced by a real yutto run.

They follow these documents of yutto v2.3.1 (https://github.com/yutto-dev/yutto):
- docs/guide/cli/auth.md          the options --auth-file and --auth-profile
- docs/guide/cli/resource.md      the options --video-only, --audio-only, --subtitle-only, --no-subtitle, ...
- src/yutto/planner (downloader/planner.py)  the output names `<name>.mp4`, `<name>.m4a`
- src/yutto/utils/subtitle.py     the subtitle name `<name>.<lang>.srt`
- src/yutto/exceptions.py         the exit codes 10 to 21

The wording of the err-*.txt lines (what yutto prints on stdout or stderr) is a guess: the first line of each is the exit code
the fake program uses, the rest is what it prints. `__SESSDATA__` and `__PROXY__` are replaced by the fake with the values it was given.
