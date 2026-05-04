# 🗃️  mailwise

Semantic email search. Describe what you're looking for in natural language and find matching emails by meaning, not just keywords. Runs entirely locally, no data leaves your machine.

## Supported clients

- **Apple Mail** — parses `.emlx` files under `~/Library/Mail/V<N>/`
- **Thunderbird** — reads mbox files under `~/Library/Thunderbird/Profiles/<profile>/ImapMail/<account>/`, honors `X-Mozilla-Status` deletion flags
- **Plain mbox archives** — point at any directory of `.mbox`/`.txt`/extension-less mboxrd dumps (Gmail Takeout, Apple Mail "Export Mailbox" output, mailing-list archives, …)

## Prerequisites

- **Apple Silicon Mac** — Metal GPU is what makes the embedder usable
- **Rust** toolchain for building from source
- For Apple Mail: **Full Disk Access** granted to your terminal (System Settings → Privacy & Security → Full Disk Access). `~/Library/Mail/` is otherwise unreadable.

## Installation

```sh
git clone https://github.com/futuun/mailwise.git
cd mailwise
cargo install --path .
```

The binary lands in `~/.cargo/bin/mailwise`. On first run, the embedding model [jina-embeddings-v5-text-nano-retrieval](https://huggingface.co/jinaai/jina-embeddings-v5-text-nano) (~230 MB, [CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/)) is downloaded into `~/.mailwise/`.

## Usage

```sh
mailwise config         # interactive: pick clients, set poll interval, etc.
mailwise index          # scan + embed (foreground)
mailwise search "..."   # natural-language query
mailwise install-agent  # background indexing via launchd; tail ~/.mailwise/logs/indexer.log
```

Initial indexing takes some time (~30 minutes for 50k emails on M1 Max). Everything is stored in `~/.mailwise/mailwise.db`. If you kill `index` mid-run it picks up where it left off next time.

`mailwise search` accepts `--open N` to open the Nth result in the configured client, and `--format json` for launcher integration (Alfred, Raycast). See `mailwise help` for the full list.

## How it works

1. **Index** — every poll, each enabled client scans messages on disk and emits `(Message-ID, locator)` pairs. The diff against `email_sources` tells us what's new, relocated, or gone. New messages get parsed (RFC 2822 + MIME body extraction; HTML through scraper/html5ever, plain text gets format=flowed unflowing + sigdash/footer trimming).
2. **Embed** — concat subject + body, embed via Jina v5 nano on Metal GPU through llama.cpp → 768-dimensional vectors.
3. **Store** — metadata in SQLite `emails`, vectors in a `sqlite-vec` virtual table with cosine distance.
4. **Search** — embed the query, KNN over the vector table, rank by cosine similarity (with a length-factor penalty so trivially-close two-word matches don't crowd out richer hits).

All data lives in `~/.mailwise/mailwise.db`.

## Adding a new client

[`MailClient`](src/clients/mod.rs) is the entire contract. Three methods do the actual work:

- `list_locators` — walk this client's data on disk and return `(Message-ID, locator)` pairs plus a `scan_complete` flag (used to refuse destructive deletes when the walk had errors). Locator format is opaque to the framework — existing clients use a `.emlx` file path (Apple Mail) or `<mbox_path>#offset=N` (Thunderbird, plain-mbox).
- `fetch_email(locator)` — parse one message at the given locator into an `Email`. Use [`parser::build_email`](src/parser/mod.rs) for the RFC 2822/MIME work.
- `open(conn, message_id)` — open the message in whatever way fits (URL scheme, native API, rendered preview).

Plus the boilerplate `source()` and `is_available()`. Add a variant to the `Source` enum, register the client in `instantiate(...)`, and [`sync_one`](src/clients/mod.rs) handles everything else: diff against `email_sources`, parallel-parse only genuinely-new Message-IDs, batched DB inserts, ratio-gated removes, end-of-cycle orphan GC across all clients.

The shared [`mbox`](src/clients/mbox.rs) module covers SIMD envelope scanning, mboxrd un-escape, and one-shot message fetch, so a new mbox-flavoured client (Mutt, Postfix Maildir-as-mbox, etc.) is mostly just a filesystem-walk predicate and an optional `should_skip` callback for client-specific deletion flags.

## License

The mailwise source code is licensed under [MIT](LICENSE).
