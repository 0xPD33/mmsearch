# mmsearch design

mmsearch is a search overlay for Wayland. It finds files in `$HOME` by path, by keyword, and by meaning. A query can be text, an image, speech, or text with an image.

## Model

mmsearch uses [google/embeddinggemma-2](https://huggingface.co/google/embeddinggemma-2). The model maps text, code, images, video and audio into one 768d vector space. It has 740M parameters: a 270M text model, a 170M vision encoder and a 300M audio encoder.

- **Runtime:** llama.cpp in-process, through the raw `llama-cpp-sys-2` bindings with the `mtmd` and `vulkan` features. The bundled llama.cpp must contain the `gemma-embedding2` architecture (llama.cpp commit `4fbc76de`, PR #30054).
- **One batch per input:** the model uses non-causal attention, and llama.cpp pools only the tokens of one `llama_process` call. The embedder therefore puts the text tokens and the media embeddings of one input into one `llama_batch_ext`. The `llama-cpp-2` safe wrapper has no API for this mixed batch, so mmsearch calls the C API directly.
- **Image budget:** the minimum and the maximum are both 280 tokens. The HF processor scales every image up to fill its budget, but llama.cpp only scales down. Without the minimum, a 640×480 image gets 130 tokens instead of 266, and its vector drifts. llama.cpp rounds the size up, so that image gets 300 tokens.
- **Weights:** `embeddinggemma-2-Q8_0.gguf` (310 MB) and `mmproj-embeddinggemma-2-Q8_0.gguf` (555 MB) from `ggml-org/embeddinggemma-2-GGUF`, at a pinned revision. The daemon downloads them on first run to `~/.local/share/mmsearch/models/`. Each file downloads to a `.part` name and gets its final name when it is complete.
- **Query prompt:** `task: search result | query: {q}`.
- **Document prompt:** `title: {path relative to $HOME} | text: {chunk}`.
- **Media inputs** get no prompt prefix. Audio must be mono 16 kHz. The context window is 8192 tokens for all modalities together.

Reasons for the in-process runtime: users install one binary, Vulkan works on all GPU vendors, and there is no second process to supervise. A `llama-server` sidecar needs a recent llama.cpp on each machine. The ONNX export needs our own image, audio and token preprocessing, and a bug there gives wrong vectors with no error.

The model card warns that float16 compute can give NaN or bad vectors. A one-time check compared the llama.cpp vectors with the reference implementation (cosine 0.99 or more for text, image, audio and text with an image). No automatic test repeats it.

## Process

One crate builds one binary, `mmsearch`.

| Command | Action |
|---|---|
| `mmsearch daemon` | Runs the indexer, the store, search and the overlay. |
| `mmsearch toggle` | Opens or closes the overlay. |
| `mmsearch status [--json]` | Shows index progress and model state. |
| `mmsearch query <text> [--image f] [--audio f] [--json]` | Searches without the overlay. |
| `mmsearch index --root <dir> --db <file>` | Indexes one directory once, with no load gate. |
| `mmsearch embed [--prompt p] [--media f]...` | Prints the vector of one input as JSON. |

The subcommands talk to the daemon over `$XDG_RUNTIME_DIR/mmsearch.sock` with newline-delimited JSON. Without `XDG_RUNTIME_DIR`, IPC fails, because a socket in `/tmp` would let other users reach the daemon. The daemon binds the socket before anything else, so a second daemon stops at once. Each side waits at most a few seconds for a line of at most 4 KB, so a silent client cannot block `toggle`. A systemd user unit starts the daemon with `Restart=on-failure`, only when `WAYLAND_DISPLAY` is set. The compositor binds `Mod+Space` to `mmsearch toggle`.

The daemon has these threads:

- **main:** the winit event loop and the overlay. It uses the SergioRibera winit fork for layer-shell, wgpu and glyphon.
- **embedder:** one OS thread owns the llama.cpp model and the mtmd context. It takes jobs from a priority queue in this order: query, live change, backlog.
- **indexer:** the walker, the inotify watcher and the content extractors.
- **search:** answers overlay queries: the fast matchers at once, the semantic matcher after the typing pause.
- **ipc**, **load** and **status:** one plain thread each. The daemon uses no async runtime.

Other threads reach the overlay through `UiSender`: a channel plus `EventLoopProxy::wake_up()`. The winit fork has no typed user events.

The embedder loads the model when work arrives. It unloads the model after 5 minutes with no queries and no indexing, which frees about 2.3 GB of VRAM. It also unloads the model when the load gate closes, so a game gets the VRAM back at once. `toggle` starts the model load at once, so the model is ready before you finish typing. Path and keyword results never wait for the model.

## Index tiers

mmsearch keeps two tiers, because most files in a typical `$HOME` are caches, game data and package stores.

- **Path tier:** the file paths under `$HOME`. It skips `.git`, `node_modules` and every hidden directory except `~/.config`; hidden files stay. The fuzzy matcher scans the whole list on each keystroke, and in one home hidden directories held 1.47M of 2.0M paths, mostly game files and package caches. Without them, a path pass takes about 15 ms instead of 0.2 to 0.9 s. The tier also follows `.gitignore`. It does not follow symlinks, and it does not enter other file systems, such as sshfs or rclone mounts.
- **Content tier:** the files that get keyword text and vectors. It also skips every dot-directory, Python venvs, `site-packages`, `node_modules`, `__pycache__` and SDK trees.

inotify cannot watch millions of paths at an acceptable cost. The daemon therefore walks the path tier again when the overlay opens, if the list is older than 60 seconds. The overlay shows the cached list at once, and new paths stream into the matcher as the walk finds them. The content tier uses inotify.

## Indexing

At startup, the walker visits the content tier and compares each file's `(mtime, size)` with the database. The mtime has nanosecond resolution. A new or changed file goes to the backlog queue. Images, video and audio go to the front of the backlog, because media search is the main use and text is most of a home directory. The daemon deletes the rows of a missing file.

inotify create, modify and remove events go to the live queue after a 2-second debounce per path, because editors save in bursts. Live jobs run at once, 4 per pass of the indexer loop. If more than 64 files are due at the same time, for example after an unzip or a git checkout, the rest go to the gated backlog. Every job skips a file whose `(mtime, size)` did not change, so a `chmod` or a `touch` costs no embedding. When a watched directory disappears, the indexer forgets its watches and deletes the rows of the files under it.

The embedder gets at most 16 text chunks per job, so a query waits for one job, not for a whole big file.

The backlog runs all the time, so new media becomes searchable without a wait for an idle session. A sampler thread checks the load every 2 seconds and closes the gate when one of these conditions is true:

- The machine is on battery, from `/sys/class/power_supply`. A `Mains`, `USB` or `Wireless` supply that is online counts as external power, and so does a machine with no system battery. Device batteries, such as the battery of a wireless mouse, do not count.
- Available memory is below 15% of the total, from `/proc/meminfo`.
- Other processes use more than 50% of all CPU cores, from `/proc/stat`. The sampler subtracts the time of the daemon and its finished child processes, such as ffmpeg.
- Other processes use more than 33% of the SMs of an NVIDIA GPU, from `nvidia-smi pmon`. The sampler ignores the daemon's own process and the compositor, because the compositor's share rises while the daemon embeds. Without `nvidia-smi`, the GPU check does not run.

After the last busy sample, the gate stays closed for 30 seconds, so a short compile or a loading screen does not toggle it. When the gate closes, the current file finishes, and the backlog and the live queue stop. Live changes wait in the queue until the gate opens. The embedder unloads the model. Only queries load it again while the gate is closed. The status shows the reason, for example `paused (gpu busy)`.

Each file kind has one extractor:

| Kind | Extractor | Index content |
|---|---|---|
| Text (1 MB or less, UTF-8, no NUL bytes) | read the file | About 500-token chunks with overlap, split on line boundaries. Each chunk gets FTS text and a vector. |
| Code (the same limits) | read the file | The same chunks. Each chunk gets FTS text. Only the first chunk gets a vector, so each code file has one vector for meaning search. |
| JSON, YAML, TOML | read the file | FTS text only. |
| Lockfiles, minified files | none | Path tier only. |
| Image | `image` crate, format from the file content (many PNG files have a .jpg name) | One vector at the default 280-token budget. Images smaller than 64 px are skipped. |
| Audio | `ffmpeg` to mono 16 kHz f32 | One vector for the first 300 seconds. |
| Video | `ffmpeg`, 8 evenly spaced frames | One vector from all 8 frames in one prompt. One frame becomes the poster thumbnail. When a seek lands past the last frame of a very short clip, the first frame stands in. A file with no video stream goes to the audio extractor. |
| PDF | `pdftotext` | The same chunks as text. |
| Other | none | Path tier only. |

Code has one vector per file. With a vector per chunk, code gave 1.2M of the 1.34M vectors in one developer's home: most of the index, of the RAM and of the first-index GPU time, and close to the limit of the vector search. Names and strings in code match better by keyword. The vector of the first chunk covers the path (from the document title), the imports and the module comment, so a meaning query can still find the file.

The embedder packs text chunks of one file into one `llama_process` call, up to 1024 tokens per call. Packed sequences still pay attention over the whole batch, so a bigger pack is slower: on an RTX 5060 Ti, 1024 tokens gave about 51 chunks per second and 8192 tokens gave about 30. Each media item gets its own call. Images index at about 5.5 files per second.

If an extractor fails, the database stores the error with the file's mtime. The indexer does not try that file again until its mtime changes. An embedding error can be temporary, for example VRAM pressure from a game, so the database stores it with a zero mtime, and the next start tries the file again. Each subprocess has a 60-second timeout, and ffmpeg and ffprobe may read local files and pipes only (`-protocol_whitelist file,pipe`).

Before the indexer embeds a file, it writes the path to `index.inflight` beside the database, and it deletes that file after. If the path is still there at startup, that file crashed the previous run: the indexer stores an error for it and skips it until it changes. This stops a crash loop under `Restart=on-failure`.

## Storage

One SQLite database at `~/.local/share/mmsearch/index.db`, in WAL mode, through `rusqlite` with the bundled SQLite. The bundled build includes FTS5. The index holds extracted document text, so the data directory has mode 0700 and the database file has mode 0600.

| Table | Columns |
|---|---|
| `meta` | key, value. Holds the model id and the schema version. |
| `files` | id, path, kind, mtime, size, error. |
| `chunks` | id, file_id, ord, text (can be null), vec (can be null). |
| `fts` | FTS5 over `chunks.text`, with prefix indexes for 2 and 3 characters. |

Text chunks have both `text` and `vec`. Images, audio and video have only `vec`.

If the model id or the schema version changes, the daemon and `mmsearch index` delete the index and build it again. Path matching keeps working during the rebuild; it reads only the stored kind of each hit. `Store::open` refuses a database that has tables but no `meta` table, so a wrong `--db` path never destroys another file. `mmsearch query` opens the index read-only and fails if the model or schema differs.

The database stores each vector at the full 768d in f16. At load, the daemon cuts each vector to its first 512 values, normalizes it, and keeps it in one contiguous matrix in RAM. Matryoshka training makes this cut valid. Because the database keeps all 768 values, a change of dimension needs no new embeddings. At 512d, multimodal quality is about 1% lower than at 768d. At 256d, it is about 5% lower.

The vector search is a brute-force dot product across 4 threads. Its limit is about 2M vectors. Above that, an HNSW index (`usearch`) is necessary.

## Search

Each keystroke runs three matchers:

1. **Path:** `nucleo` matches the query against the path list and returns the top 200.
2. **Keyword:** FTS5 with BM25 returns the top 200. Each word of the query is quoted, so typed input cannot break the FTS query syntax. A word of 2 or more characters is a prefix term. A one-character word matches whole tokens only, because a one-character prefix scans most of the index.
3. **Meaning:** after a 150 ms pause in typing, the embedder embeds the query prompt. The matrix scan returns the top 200.

Each list collapses to the best rank per file before the merge, so one long file cannot fill the results. Reciprocal Rank Fusion merges the lists: score = Σ 1 / (60 + rank). A file in several lists goes to the top. The filter chips remove kinds before the merge. The overlay gets all merged files, at most 600; building them takes about 5 ms.

The daemon answers a keystroke in three passes, and each pass sends a full list:

1. **Paths only**, in about 15 ms.
2. **Paths and keywords.** FTS5 ranks every prefix match before it keeps the top 200, so this pass takes 0.1 to 0.5 s over about 2M chunks. If a newer keystroke is already waiting, the daemon skips this pass.
3. **All three matchers**, after the typing pause.

Before the user moves the selection, each new list selects its top result. After a move, the selected file keeps its place on screen while the list reorders around it.

An image or a voice query uses only the meaning matcher. Text with an attached image gives one interleaved vector from one prompt. The text part of that query also runs the path and keyword matchers.

All lists have equal weight. A semantic list always returns 200 results, so it can push weak matches up. If exact path matches start to lose, add a list weight or a similarity floor.

## Overlay

winit cannot hide a Wayland surface. The daemon therefore keeps the GPU device, the pipelines and the glyph atlas alive, and it creates and destroys only the layer surface on each toggle. These parts are the expensive ones, so the overlay opens in one frame.

The surface is a centered panel on `Layer::Overlay`, with exclusive keyboard focus. The layout code works in a 900×560 logical-pixel panel, and the surface is 1.2 times that by default. `Ctrl+=` and `Ctrl+-` change the zoom from 0.8 to 1.8 until the daemon restarts. A layer surface gets its size when it is created, so a zoom change closes the surface and opens it again with the same state. The layout has a search field, filter chips, a result list on the left, a preview pane with an action bar on the right, and a line of key hints. With no query, the list shows example queries, which a click runs, and the preview pane shows the key reference.

| Key | Action |
|---|---|
| typing | Edits the query at the caret. `←` `→` `Home` `End` move the caret, `Shift` selects, and `Ctrl` moves or deletes by word. `Ctrl+A`, `Ctrl+X`, `Ctrl+Z` and `Ctrl+Shift+Z` work as in any text field. Plain typing joins one undo step until a separator. |
| `↑` `↓`, `Ctrl+J` `Ctrl+K`, `PgUp` `PgDn`, `Ctrl+Home` `Ctrl+End` | Moves the selection. |
| `Tab`, `Shift+Tab`, `Ctrl+1` … `Ctrl+5` | Changes the filter: All, Images, Media, Docs, Code. |
| `Enter` | Opens the file with `xdg-open` under `setsid`. |
| `Ctrl+Enter` | Opens the folder with the file selected, through `org.freedesktop.FileManager1.ShowItems`. |
| `Ctrl+C` | Copies the selected query text, else the path, with `wl-copy`. |
| `Ctrl+Shift+C` | Copies the file: image data with its MIME type for an image up to 50 MB, else a `text/uri-list` entry. |
| `Ctrl+V` | Attaches an image chip if the clipboard holds an image. Otherwise it pastes text at the caret. |
| `Ctrl+S` | Hides the overlay, selects a region with `slurp`, captures it, and reopens with the region as an image chip. The capture uses `ext-image-copy-capture-v1`, or `zwlr_screencopy_manager_v1` when the compositor lacks it; niri 26.04 offers only the wlr protocol. |
| hold `Ctrl+Space` | Records a voice query with `cpal`, mono 16 kHz, 30 seconds maximum. A level bar shows in the search field. |
| `Esc` | Closes the overlay. |

A held key repeats editing and movement only. Escape, Enter, Tab and `Ctrl` letter combinations never repeat, because winit keeps repeating a held key after the surface is gone.

The mouse can do everything the keys do:

- Pointer motion over a row selects it. A list that scrolls under a resting pointer keeps its selection, because only real motion counts.
- A click on a row or on the preview opens the file. A right click shows it in its folder.
- The wheel scrolls the list by 2 rows per notch, with an ease of about 45 ms; a touchpad scrolls without easing. When the selection would leave the view, it moves to the nearest fully visible row. Over the preview pane, the wheel scrolls long preview text.
- The scrollbar thumb drags, and a click on the track jumps there.
- The action bar has Open, Show in folder, Copy path and Copy file.
- In the search field, a click places the caret, a drag selects, and a double click selects a word. An × clears the query, and a click on an image or voice chip removes it.
- The pointer shape shows what a click does: a hand over buttons and rows, a text cursor over the field.

Each result row shows a thumbnail for an image or a video, else a colored file-type badge, the file name, the parent path relative to `~`, and the modification age ("5 min", "3 d", then a date). A text hit also shows one snippet line: the first line of the best chunk that contains a query word.

A worker thread loads row thumbnails for the visible rows and one page ahead, newest request first, from the freedesktop cache; for an image that is not indexed yet, it makes the thumbnail. Up to 1500 row thumbnails of 96 px stay on the GPU. Images in the scene carry a scissor rect, so a half-visible row cannot draw over the filter chips.

The indexer writes a thumbnail for each image and video to the freedesktop cache in `~/.cache/thumbnails`, which file managers share. It uses the GLib URI escaping for the file name hash, and modes 0600 and 0700, as the spec asks. The preview pane uses the cached poster frame for a video. One preview worker decodes previews and skips to the newest request. For a photo, it sends the cached 256 px thumbnail first, and it skips the full decode if the selection moves on. An image chip is scaled to a 1536 px long edge when it is attached. Apps that Enter opens do not inherit the wrapper's `LD_LIBRARY_PATH`.

| Kind | Preview |
|---|---|
| Image | The image, fit to the pane. |
| Text and code | The best chunk. |
| PDF | The extracted text. |
| Audio | Size and modified time. |
| Video | The poster frame. |

The status line shows index progress, for example `12,340 / 157,000 · paused (gpu busy) · unloaded`. `mmsearch status --json` gives the same data to a status bar.

A voice query embeds the speech directly, with no speech-to-text step. If speech vectors match text documents badly, the fallback is speech-to-text through `speechcore`, which turns the speech into a text query.

## Errors

- If Vulkan fails, llama.cpp runs on the CPU. The status line does not show this.
- If the model is missing or the download fails, semantic search is off. Path and keyword search keep working, and the status line shows the reason.
- If `toggle` finds no daemon, it calls `notify-send` and exits with status 1.

## Packaging

A Nix flake builds the binary with `buildRustPackage` and the rust-overlay toolchain.

- **Build inputs:** cmake, shaderc, vulkan-headers, spirv-headers, libclang (through `bindgenHook`), wayland, libxkbcommon, alsa-lib. The winit fork and the llama-cpp-rs fork are git dependencies with fixed `outputHashes` entries. The llama-cpp-rs fork (`0xPD33/llama-cpp-rs`, branch `embeddinggemma2`) only moves the llama.cpp submodule to a commit with `gemma-embedding2`. It goes away when a `llama-cpp-sys-2` release on crates.io includes that architecture.
- **Runtime `PATH` in the wrapper:** ffmpeg-headless, poppler-utils (`pdftotext`), slurp, wl-clipboard, xdg-utils, curl (model download), util-linux (`setsid`), systemd (`busctl`), libnotify (`notify-send`).
- **systemd:** the package installs `lib/systemd/user/mmsearch.service` with an absolute `ExecStart`.

## Verification

`tests/e2e.sh` does an end-to-end check and writes a report that a person can read.

1. It indexes `tests/fixtures` with `mmsearch index`. The fixtures are small CC0 files: photos, a note, a source file, a PDF, an audio clip and a video.
2. It runs fixed text, image and audio queries with `mmsearch query --json`. Each query expects its target file in the top 3.
3. It writes `e2e-report.html` with each query, its top 5 results, their scores and thumbnails. With `MMSEARCH_SCREENSHOT=1` in a niri session, it also starts an isolated daemon and adds a `grim` screenshot of the overlay panel only, 1080x672 at the default zoom. `XDG_CACHE_HOME` points into the temp directory, so the test writes no thumbnails to the real cache.

## Not included

OCR, office and epub text extraction, the audio track of a video, audio after 300 seconds, drag and drop, closing on a click outside the panel, highlighting of matched words, syntax highlighting, HNSW and X11.
