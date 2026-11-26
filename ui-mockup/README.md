# Console UI prototype

Interactive prototype of the redesigned zori console (see
[`.omut/threads/console-ui-redesign.md`](../.omut/threads/console-ui-redesign.md)).
Fake data in `data.js` is shaped like `GET /api/runs/:id`; fields marked
EXTRA are what the backend would have to add. A small simulation ticks the
running nodes so the cards, sparklines and timeline move.

Run: `python3 -m http.server` in this folder, open the printed URL.

## Layout (IDE-style tool windows)

The canvas is always in the middle; tool-window buttons on the edges open
panels (click again to close, drag the panel edge to resize):

| strip | windows | keys |
|---|---|---|
| left, top | Runs · Pipelines · Fleet (harness × model) · Artifacts | ⌥1–⌥4 |
| left, bottom | Timeline (attempts Gantt) · Logs (what `zori run` prints) | ⌥5–⌥6 |
| right | Node (inspector + chat with the node's agent) · Planner (supermodel chat) · Attention inbox | ⌥7–⌥9 |

## Canvas

- Drag a harness·model from **Fleet** onto the canvas: the chip turns into a
  card outline, and dropping it materialises a draft node. Dropped right of a
  card's output it wires itself to that card. A click without dragging adds
  it after the selected node.
- Pull a card's **●** output onto another card to wire it (the target reads
  `${steps.<from>.output}`); cycles and duplicates are refused. Releasing on
  empty space opens a picker for a new node there.
- Double-click empty space → add node; drag to pan; pinch or ⌘+wheel to zoom;
  shift-click to multi-select; ⌫ deletes; ⌘Z undoes; F fits.
- Drafts get a **Run N new** button in the top bar.
- ⌘K is a command palette (commands, nodes, "add node: harness · model").
- **Compare** shows sibling agents side by side: metrics, final report,
  "use this result".

## Foundation (D14)

Native ES modules, no bundler, no npm: Preact + htm come vendored as one
13 KB file (`vendor/preact-htm.js`: htm 3.1.1, Apache-2.0, bundling Preact 10, MIT), so the files can
later be embedded in the binary as they are.
