# Transparency fixtures (Story 18.2)

## `FIXTURE_room_journal_pre_18_2.jsonl`

A room journal in the **pre-Story-18.2 on-disk shape** — exactly what
`rustain@6ca43eb` wrote to `{workspace}/.rustain/rooms/room-<hash>.jsonl`:

- no `recorded_at_ms` on the `JournalEntry` envelope;
- no `direction` on `remote_envelope_accepted` / `remote_envelope_rejected`.
- no persisted `task` correlation on either remote-envelope event.

It exists because two of Story 18.2's assertions **cannot fail without it**, and
an assertion that cannot fail is one that quietly gets skipped:

1. **Forward compatibility (AC2).** A journal written before the schema change
   must still parse, must still project, and its missing fields must render as
   explicit unknowns (`—`, `direction: unknown`) rather than epoch zero or a
   fabricated direction. Deleting either `#[serde(default)]` turns
   `a_pre_18_2_journal_still_parses_and_renders_missing_fields_as_explicit_unknowns`
   RED.

2. **Unknown-variant rendering (AC5, UX-DR-ROOM-01).** Line 9 carries the event
   tag `room_unrecognised_marker`, which **this build has no arm for** — an
   invented tag that names no planned feature, so no future build can ever
   grow an arm for it and repeat the failure below. That is legal:
   ⚑ Amended 2026-08-20 by Story 18.4a: this line carried `peer_equivocated`
   from 18.2 until 18.4a **shipped that variant**, at which point the fixture's
   unknown representative became a known arm and the AC5 assertion could no
   longer fail. The representative moved, one line, byte-identical shape — the
   `== 7` row counts and every other line are untouched. ⚑ Amended again the
   same day on code review (D6): the first replacement tag named a real
   deferred crypto-cluster event, which the story's scope fence forbids as a
   *string* — and any real planned tag would someday become known and repeat
   the failure, so the representative is now a tag that names nothing.
   `RoomEvent` is `#[non_exhaustive]`, internally tagged, and declares
   `#[serde(other)] Unrecognized`. You cannot assert "unknown variants render
   explicitly" against a variant that does not exist yet, so the fixture
   supplies one. Replacing the unknown arm with a silent `continue` turns
   `an_unrecognised_event_tag_renders_an_explicit_unknown_row` RED.

### Legacy compatibility coverage

The pre-18.2 fixture still exercises every projected variant, but its envelope
directions are intentionally absent and deserialize as `unknown`. It is
therefore evidence for replay compatibility and unknown rendering — **not**
P-7's persisted-direction export evidence.

### `FIXTURE_room_journal_current_18_2.jsonl` — P-7 export evidence

Byte-identical regeneration passes on an empty journal and proves nothing on
its own. This separate current-shape fixture carries nonlegacy timestamps,
persisted `direction`, and persisted original `task` values:

| seq | persisted event | direction | projects? |
|-----|-----------------|-----------|-----------|
| 1 | `remote_envelope_accepted` | inbound | accepted |
| 2 | `remote_envelope_rejected` | inbound | refused |
| 3 | `remote_envelope_accepted` | outbound | accepted |
| 4 | `remote_envelope_rejected` | outbound | refused |
| 5 | `admission_deferred`, `a2a-inbound-approval:` | derived inbound | awaiting-approval |
| 6 | `admission_deferred`, `a2a-status-query:` | derived inbound | status-query |
| 7 | unmapped future event tag | unknown | unknown |
| 8 | `recipient_item_retracted` (appended by Story 19.16d; `principal_collapsed: true`) | derived inbound | item-retracted |

The export keystone asserts every row's `(seq, kind, direction, task)` before
it checks deletion/corruption regeneration. Dropping an envelope arm,
discarding either persisted direction, or using opaque node-id suffixes in
place of original task correlation changes the expected shape and turns it
RED.

### Peer ids

Multihash-encoded SHA-256 handles. `1220…4bb0…` and `1220…a1b2…` are arbitrary
test values, not captured material — nothing here is a secret and nothing here
was fetched from a live host.

### Reproducing

`./REPRODUCE_transparency_fixture.sh` regenerates only the historical
pre-18.2 fixture and re-pins its manifest entry. The current-shape fixture is
deliberately pinned separately: the regression needs exact persisted inbound
and outbound directions rather than whatever a future emitter happens to
produce.
