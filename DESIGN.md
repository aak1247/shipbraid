---
name: Shipbraid Dashboard
description: A compact release coordination ledger for tracking requests, executions, and verification evidence.
colors:
  ground: "#f2f3f1"
  surface: "#fbfcfa"
  raised: "#fff"
  ink: "#17221f"
  muted: "#64716c"
  soft-muted: "#79847f"
  line: "#d8dfdb"
  line-strong: "#c6d0cb"
  accent: "#176b56"
  accent-soft: "#e4efea"
  urgent: "#aa4b1f"
  urgent-soft: "#fff1e7"
  attention: "#8d5a0c"
  success: "#28714e"
  failure: "#a73e3e"
  attention-soft: "#fbf1dc"
  success-soft: "#e7f3eb"
  failure-soft: "#f9e9e7"
typography:
  body:
    fontFamily: "system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans SC', 'Microsoft YaHei', sans-serif"
    fontSize: "14px"
    fontWeight: 400
  label:
    fontFamily: "system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans SC', 'Microsoft YaHei', sans-serif"
    fontSize: "12px"
    fontWeight: 550
  data:
    fontFamily: "SFMono-Regular, Consolas, Liberation Mono, monospace"
    fontSize: "12px"
    fontWeight: 400
rounded:
  xs: "3px"
  sm: "4px"
  control: "5px"
  md: "6px"
  lg: "8px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "24px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.raised}"
    rounded: "{rounded.control}"
    padding: "0 14px"
  input-token:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "0 9px"
  chip-status:
    backgroundColor: "#eef1ef"
    textColor: "#5a6861"
    typography: "{typography.label}"
    rounded: "{rounded.sm}"
    padding: "2px 6px"
  nav-lane-selected:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "11px 10px 9px"
---

# Design System: Shipbraid Dashboard

## Overview

**Creative North Star: “The Dispatch Ledger”**

The Dispatch Ledger treats release coordination as a record-keeping job. Stable rows, aligned identifiers, narrow state labels, and a clear request-to-run link make it quick to scan what is waiting and inspect what actually happened. The interface is quiet and precise, with green reserved for connection, selection, and verified success; warm colors identify urgency or uncertainty.

The ledger is the primary working surface. Requests and executions remain separate records, while the inspector carries their relationship, phase history, receipt, and bounded logs. It uses only records returned by the service and keeps unresolved ownership explicit. Its compact operational density is a deliberate fit for developer and release-owner workflows.

**Key Characteristics:**
- Request and execution identity remain distinct.
- State color always appears with a readable label.
- Borders, alignment, and surface tone carry most of the hierarchy.
- Verification details remain traceable to their source execution.

## Colors

The palette uses cool, low-chroma paper and graphite neutrals with a muted green accent. State colors stay local to the state they explain.

### Primary
- **Ledger Green**: The accent marks the selected lane, connected state, keyboard focus, and primary connection action.
- **Accent Wash**: A pale green surface separates the selected row or control from the neutral ledger paper.

### Secondary
- **Urgent Rust**: Urgent request priority uses this warm signal and a text label.
- **Urgent Wash**: The pale rust surface supports the urgent label without replacing its text.
- **Attention Ochre**: Unknown execution state, blocked lanes, and cancel requests use this signal alongside explanatory copy.
- **Attention Wash**: A pale ochre surface supports uncertain or blocked states.

### Tertiary
- **Verified Green**: Successful execution and receipt results use this stronger confirmation color.
- **Success Wash**: The pale green surface supports verified success labels.
- **Failure Red**: A determined failed execution or receipt uses red with an explicit failure label.
- **Failure Wash**: The pale red surface supports failure labels.

### Neutral
- **Ground**: The page canvas sits behind the working regions.
- **Ledger Paper**: The request and execution table use this light surface for long-form scanning.
- **Raised Paper**: Selected rows and interactive controls use the clearest surface.
- **Graphite Ink**: Primary text, identifiers, and high-contrast labels.
- **Soft Graphite**: Supporting labels, timestamps, and secondary instructions.
- **Fainter Graphite**: Less prominent timestamps and metadata.
- **Divider Gray** and **Strong Divider Gray**: Table rules and panel boundaries define structure without heavy framing.

**The Paired-State Rule.** A state color never carries meaning alone; retain the status word or an explanatory sentence beside it.

## Typography

**Display Font:** `system-ui`, with platform and Chinese-capable fallbacks
**Body Font:** `system-ui`, with Noto Sans SC / Microsoft YaHei fallbacks
**Label/Mono Font:** SFMono-Regular, Consolas, Liberation Mono

**Character:** Interface copy stays neutral and compact. Monospace marks IDs, credentials, revisions, digests, cursors, timestamps, and executor log text, where exact characters and aligned numerals matter.

### Hierarchy
- **Page heading** (650, 21–23px): Names the active ledger view without a decorative eyebrow.
- **Section title** (600–650, 12–15px): Labels inspector sections and working controls.
- **Body** (400, 14px default): Carries readable instructions and connection guidance.
- **Label** (500–600, 12–14px): Supports dense table metadata and state labels; compact labels remain at least 12px.
- **Data** (400, 12px): Shows exact IDs, revisions, timestamps, and digests with tabular numerals.

**The Data-Only Mono Rule.** Keep monospace on machine identifiers, credentials, numeric evidence, and executor log text; use the interface sans for explanatory text.

## Layout

The app uses a narrow lane rail and a flexible ledger. Selecting a record opens a non-modal inspector over the right side of the ledger without reflowing the table. Its width is up to 1080px; on intermediate screens it preserves the lane rail, and on phones it fills the viewport. The pinned inspector header exposes Close; Escape closes it and returns focus to the selected record. Background controls remain available on desktop.

Execution records open on logs. Context, receipts and related requests remain accessible in a disclosure above the feed, while the timeline remains a separate view. The log viewport consumes the available inspector height and scrolls independently. Short windows allow the inspector body to scroll so controls remain reachable. On phones, each semantic ledger row retains its stacked arrangement without page-level horizontal scrolling.

## Elevation & Depth

Depth comes primarily from surface tone and thin borders. Selected rows and controls use a very restrained 1px shadow only when their state benefits from separation; large ambient shadows are absent. Focus is conveyed with a clear outline rather than elevation.

**The Flat-By-Default Rule.** Use borders and surface changes for structure; reserve shadows for compact selected controls.

## Shapes

The form language is softly squared: compact controls use 4–6px corners, panels use 6–8px, and status chips stay nearly square. Fine separators provide the dominant geometry. Avoid pill-shaped controls that compete with the ledger’s tabular rhythm.

## Components

### Buttons
- **Primary connection action:** Solid ledger green, white text, gently squared corners; hover darkens the surface and keyboard focus uses a visible outline.
- **Secondary controls:** Quiet surfaces with a one-pixel border. Refresh and clear use compact icon buttons with explicit accessible names.

### Inputs
- **Token field and filters:** Raised paper, graphite text, and a fine gray border. Focus adds a green outline; password visibility is a separate, labeled button.

### Navigation
- **Lane rail:** Project/environment filters, plain text, a small state marker, and the lane's executor label. The selected lane receives a raised surface and a narrow green leading rule; disabled lanes remain visible but muted with a textual disabled state.
- **Record and detail controls:** Native button groups distinguish requests from runs and switch between event history and logs. The selected button exposes `aria-pressed`; these controls are not tab widgets.

### Chips
- **Status badges:** A small dot and readable state label share a pale state-tinted surface. Priority is written out, with rust reserved for urgent items.

### Ledger rows
- **Request and run rows:** Semantic table columns align revision, status, priority, relationship, and time. Hover and selection use subtle surface changes; the active row also receives a leading accent rule. On phones, each row stacks these fields without page-level horizontal scrolling.

### Record inspector
- **Detail sections:** Use headings and thin separators rather than nested cards. Related records are explicit links, while receipts expose source revision, observation time, digests, and the verification report.

### Live feeds
- **Event timeline and executor logs:** Show bounded records with timestamps and sequence numbers. Logs use a graphite-green terminal surface, compact monospace rows, level text/color, and an independently scrollable viewport. Tail follows appended records by default; scrolling up or pressing Pause freezes the visible window while polling continues. Resume / Back to bottom displays the latest rolling window. A wrap toggle preserves long lines on desktop and defaults on for phones. Keep freshness and cursor context visible, never animate streamed text or announce every line to screen readers. The ledger states `最近 200 条 · 筛选当前列表`; filters apply to the list currently returned to the page.
- **Executor labels:** Identify the executor for the selected lane. Label ScriptedExecutor lanes and synthetic receipt headings and outcomes as simulated, and keep each report tied to its source run.

## Do's and Don'ts

### Do
- Keep the request and execution ledgers visibly distinct.
- Show the request’s current run or the historical run that satisfied it.
- Keep historical relationships scoped to records and event history the service has returned and the dashboard has read.
- Make unknown ownership and superseded execution status explicit in words.
- Keep IDs, revisions, and receipt evidence easy to copy and inspect.
- Respect reduced-motion preferences and visible keyboard focus.

### Don't
- Don't invent sample records, summary metrics, or activity in the live view.
- Don't imply that a superseded run means its request failed.
- Don't use state colors without text labels.
- Don't make the page scroll horizontally on a phone.
- Don't persist bearer tokens in browser storage.
