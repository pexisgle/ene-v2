---
name: ene-desktop
description: Strawberry Milk Capsule desktop companion UI for GPUI
world: grounded-milk
colors:
  milk-surface: "#fffdfd"
  strawberry-milk: "#fadce6"
  blush-wash: "#fff4f8"
  user-petal: "#fce2ec"
  berry-accent: "#cf2e68"
  berry-hover: "#b52358"
  berry-soft: "#fce4ee"
  berry-soft-ink: "#7a1c40"
  mint-live: "#229a73"
  mint-soft: "#ddf5ec"
  mint-ink: "#16694f"
  amber-warn: "#c97a16"
  amber-soft: "#fff3df"
  amber-ink: "#7c4508"
  rose-danger: "#c92a52"
  rose-danger-soft: "#ffe9ee"
  rose-danger-ink: "#9e1c3e"
  cocoa-ink: "#3b232c"
  plum-muted: "#6b3b4e"
  plum-placeholder: "#7d4b5e"
  petal-border: "#f3c4d5"
  milk-divider: "#f2bdd0"
  float-outline: "#f09bb8"
typography:
  section-heading:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "22px"
    fontWeight: 700
    lineHeight: 1.35
    letterSpacing: "0em"
  subsection-heading:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "18px"
    fontWeight: 700
    lineHeight: 1.4
    letterSpacing: "0em"
  body:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: "0em"
  row-title:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "14px"
    fontWeight: 700
    lineHeight: 1.4
    letterSpacing: "0em"
  pill-label:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "12px"
    fontWeight: 700
    lineHeight: 1.3
    letterSpacing: "0em"
  caption:
    fontFamily: "Yu Gothic UI, Hiragino Sans, Meiryo, BIZ UDPGothic, Noto Sans CJK JP, sans-serif"
    fontSize: "11px"
    fontWeight: 500
    lineHeight: 1.3
    letterSpacing: "0em"
rounded:
  xs: "4px"
  sm: "10px"
  md: "14px"
  lg: "18px"
  bubble: "24px"
  pill: "9999px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "24px"
  xxl: "28px"
components:
  button-primary:
    backgroundColor: "{colors.berry-accent}"
    textColor: "{colors.milk-surface}"
    typography: "{typography.pill-label}"
    rounded: "{rounded.pill}"
    padding: "6px 14px"
  button-primary-hover:
    backgroundColor: "{colors.berry-hover}"
  button-soft:
    backgroundColor: "{colors.berry-soft}"
    textColor: "{colors.berry-soft-ink}"
    typography: "{typography.pill-label}"
    rounded: "{rounded.pill}"
    padding: "6px 14px"
  button-ghost:
    backgroundColor: "{colors.milk-surface}"
    textColor: "{colors.plum-muted}"
    typography: "{typography.pill-label}"
    rounded: "{rounded.pill}"
    padding: "6px 14px"
  orb-button:
    backgroundColor: "{colors.milk-surface}"
    textColor: "{colors.plum-muted}"
    rounded: "{rounded.pill}"
    size: "40px"
  input-field:
    backgroundColor: "{colors.milk-surface}"
    textColor: "{colors.cocoa-ink}"
    typography: "{typography.body}"
    rounded: "{rounded.sm}"
    padding: "8px 12px"
  settings-card:
    backgroundColor: "{colors.milk-surface}"
    textColor: "{colors.cocoa-ink}"
    rounded: "{rounded.md}"
    padding: "16px"
---

# Design System: ene-desktop

## Overview

**Creative North Star: "Strawberry Milk Capsule"**

`ene-desktop` treats the desktop companion interface as a warm strawberry-milk cutout floating gently above arbitrary desktop wallpapers, paired with a calm, uncluttered three-chapter settings studio. Everyday voice and text turns, first-run onboarding, and safety approvals resolve inside an asymmetric transparent-window speech pod (`CompanionPod`) anchored at bottom-right with a single-line capsule composer and a detached controls menu button.

Complexity is strictly hidden behind progressive disclosure and clear standard Japanese labels: nested bordered cards, uppercase eyebrow kickers, Unicode glyph icons, raw diagnostic dumps, memory tables, and childish copy are explicitly rejected in favor of crisp vector SVG icons (`apps/ene-desktop/src/ui/icons.rs`), `>= 4.5:1` cocoa-and-plum contrast on pure milk surfaces, and single-line hairline rows in `SettingsStudio`.

**Key Characteristics:**
- Dual-window separation: frameless transparent `CompanionPod` (`444x218` mini / `444x540` expanded) for live dialogue and `SettingsStudio` (`780x580`) for infrequent configuration.
- Zero nested bordered cards: structured containers use a single `1px` petal border (`#f3c4d5`) with hairline dividers or flat blush strips (`#fff4f8`) inside.
- Progressive disclosure by default: 3-step inline onboarding disappears once configured, at most one row expands at a time, and 7-role model slot overrides stay collapsed under `詳細設定`.
- Tactile capsule silhouette paired with crisp stroke SVG iconography (`Icon::ALL`) across every control and window chrome button.

## Colors

The palette pairs pure warm milk whites (`#fffdfd`) and strawberry-milk washes (`#fadce6`, `#fff4f8`) with high-contrast berry rose (`#cf2e68`), deep cocoa ink (`#3b232c`), and clear mint/amber/rose state hues.

### Primary
- **Berry Accent** (`#cf2e68` / `oklch(55% 0.21 356)`): Primary pill buttons, overlapping floating pod status badge, active sidebar navigation pill, focused field border, and active send button.
- **Berry Hover** (`#b52358` / `oklch(48% 0.20 356)`): Hover fill for primary actions and active orbs.
- **Berry Soft** (`#fce4ee`) / **Berry Soft Ink** (`#7a1c40`): Soft pill button fill (`Tone::Soft`), active secondary selections, and high-contrast berry text (`> 8:1`).

### Secondary
- **Mint Live** (`#229a73`) / **Mint Soft** (`#ddf5ec`) / **Mint Ink** (`#16694f`): Live connection dots, active companion status, active mic toggle tint, and `有効` / `接続中` status pills.

### Tertiary
- **Amber Warn** (`#c97a16`) / **Amber Soft** (`#fff3df`) / **Amber Ink** (`#7c4508`): Connecting/unverified connection states (`未確認`, `接続中…`) and revision-conflict notices (`再読み込み`).
- **Rose Danger** (`#c92a52`) / **Rose Danger Soft** (`#ffe9ee`) / **Rose Danger Ink** (`#9e1c3e`): Two-step destructive actions (`削除`, `拒否`), close orb hover wash, and error banners.

### Neutral
- **Milk Surface** (`#fffdfd`): Primary opaque panel and floating bubble ground.
- **Strawberry Milk** (`#fadce6`): `38px` Settings titlebar, `196px` left chapter sidebar, and `kind_pill` tag background.
- **Blush Wash** (`#fff4f8`): Inset disclosure strips inside expanded cards, disabled switch track, and inactive timeline assistant bubbles.
- **User Petal** (`#fce2ec`): User message bubbles inside expanded chat history.
- **Cocoa Ink** (`#3b232c`): Primary headings, body copy, and row titles on light milk grounds.
- **Plum Muted** (`#6b3b4e`) & **Plum Placeholder** (`#7d4b5e`): Secondary metadata, labels, and field placeholders (`>= 4.5:1` contrast against `#fffdfd`).
- **Petal Border** (`#f3c4d5`), **Milk Divider** (`#f2bdd0`), **Float Outline** (`#f09bb8`): `1px` Settings row/card strokes, titlebar/sidebar dividers, and `2px` cutout outlines for floating pod elements.

### Named Rules
**The Explicit State Word Rule.** Every connection row, approval state, and device toggle pairs its status dot or pill color with an explicit Japanese state word (`接続中`, `未確認`, `接続エラー`, `無効`, `使用中`, `登録済み`, `未登録`).
**The Cocoa Ink Contrast Rule.** Body copy, muted metadata, and placeholders stay anchored in Cocoa/Plum tones (`#3b232c`, `#6b3b4e`, `#7d4b5e`) over `#fffdfd` and `#fadce6` so contrast stays at or above 4.5:1 without muddy gray text.

## Typography

**Display Font:** `Yu Gothic UI` (fallbacks: `Hiragino Sans`, `Meiryo`, `BIZ UDPGothic`, `Noto Sans CJK JP`)
**Body Font:** `Yu Gothic UI` (fallbacks: `Hiragino Sans`, `Meiryo`, `BIZ UDPGothic`, `Noto Sans CJK JP`)
**Label/Mono Font:** Same native Japanese UI stack with tabular clarity for numeric IDs and endpoints.

**Character:** Clear, warm Windows Japanese system typography relying strictly on weight (`700` vs `500`/`400`) and clean size steps rather than decorative display faces or uppercase tracking.

### Hierarchy
- **Display / Section Heading** (`700`, `22px`, `1.35`): Chapter titles (`コンパニオン`, `接続`, `この端末`) in `SettingsStudio`.
- **Headline / Subsection Heading** (`700`, `18px`, `1.4`): Secondary section headers such as `許可ルール`.
- **Title / Row Title** (`700`, `14px`, `1.4`): Companion/connection row labels and device capability settings.
- **Body** (`400`, `14px`, `1.5`): Dialogue speech text, timeline messages, and single-line inputs.
- **Label / Pill** (`700`, `12px`, `1.3`): Capsule buttons, status pills, kind tags, and field labels.
- **Caption** (`500`, `11px`, `1.3`): Subtle secondary hints inside roster rows and expanded history bar.

### Named Rules
**The Zero Kicker Rule.** Section headings stand alone with optional right-aligned primary pill actions; decorative eyebrow kickers and subtitle paragraphs above headings are forbidden.

## Layout

Two purpose-built GPUI windows separate high-frequency companionship from low-frequency configuration:
- **`CompanionPod`** (`444x218` `MiniVoice`, expanding to `444x540` `ExpandedChat`) uses a transparent bottom-right window, an asymmetric speech bubble with a `12px` tail dot, and a bottom row with a `44px` composer for typing and sending. A detached `40px` menu button opens companion switching, microphone, speaker, history, settings, and close controls above the row.
- **`SettingsStudio`** (`780x580`, minimum `680x480`): Bounded window with a `38px` Strawberry-Milk custom titlebar (`設定 — ene` + SVG minimize/maximize/close), a `196px` left chapter sidebar (`コンパニオン`, `接続`, `この端末`), and a scrollable `#fffdfd` content pane (`28px` horizontal / `22px` vertical padding, `16px` vertical section stack, `62px` list rows with `10px` vertical gaps and at most one expanded row per section).

## Elevation & Depth

`ene-desktop` stays completely flat inside bounded settings chrome while reserving a single crisp cutout drop shadow strictly for the frameless transparent `CompanionPod` floating over desktop wallpapers.

### Shadow Vocabulary
- **`float-cutout`** (`box-shadow: 0 4px 10px hsla(340, 45%, 14%, 0.16)`): Single ambient lift applied alongside a `2px` `#f09bb8` cutout stroke only to floating transparent-window islands (`CompanionPod` speech bubble, `12px` tail dot, composer capsule, `40px` orb buttons, and companion switcher popover).

### Named Rules
**The Single-Boundary Elevation Rule.** Elements inside `SettingsStudio` never carry shadows or nested bordered boxes; sections expand via a `1px` `#f3c4d5` hairline divider (`border_t_1`) and flat `#fff4f8` inset strips.

## Shapes

Every surface balances soft circularity against anchored structure:
- **`CompanionPod` speech bubble**: Asymmetric silhouette with `24px` top-left/top-right/bottom-left corners and an `8px` bottom-right pointer corner pointing toward a `12px` circular tail dot (`border_2` `#f09bb8`).
- **Status/Name floating badge**: Overlapping `9999px` berry pill (`#cf2e68`) positioned `-13px` across the speech bubble's top-left corner edge.
- **Pills & Orbs**: Capsule buttons, toggle tracks (`40x22px`), status tags, kind tags, composer dock (`h: 44px`), and detached orbs (`40x40px`) use `9999px` full rounding.
- **Settings containers & inset strips**: List cards use `14px` corners; inner `#fff4f8` disclosure strips and inputs use `10px` corners; chat bubbles use `14px` with a `4px` corner accent toward speaker side.

## Components

### Buttons
- **Shape:** Full capsule (`9999px`) for `pill_button` (`padding: 6px 14px`, `gap: 6px`) and circular `circle_icon_button` / `floating_orb_button`.
- **Primary:** Solid Berry Accent (`#cf2e68`) with crisp `#fffdfd` bold label and optional `13px` SVG leading icon.
- **Hover / Focus:** Transitions to `#b52358` on primary pills; Soft/Mint hover washes to `0.84` alpha; Ghost hovers to Blush Wash (`#fff4f8`).
- **Secondary / Ghost / Danger:** `Tone::Soft` (`#fce4ee` fill, `#7a1c40` text, `1px #f3c4d5`), `Tone::Ghost` (`#fffdfd` fill, `#6b3b4e` text, `1px #f3c4d5`), and `Tone::Danger` (`#ffe9ee` fill, `#9e1c3e` text, `1px` translucent `#c92a52`).

### Chips
- **Style:** `status_pill` (`padding: 3px 10px`, `9999px`, optional `6px` circular live indicator dot) and `kind_pill` (`#fadce6` fill, `#6b3b4e` text, `12px` bold).
- **State:** `Mint` (`接続中`, `有効`, `使用中`), `Amber` (`未確認`, `接続中…`, `確認`), `Danger` (`接続エラー`, `拒否`, `利用不可`), and neutral `Blush Wash` (`無効`, `未登録`).

### Cards / Containers
- **Corner Style:** `14px` rounded containers inside `SettingsStudio`.
- **Background:** `#fffdfd` surface with optional flat `#fff4f8` inset panels when expanded.
- **Shadow Strategy:** None in `SettingsStudio`; `float-cutout` strictly on floating pod islands.
- **Border:** `1px` `#f3c4d5` on Settings cards; `2px` `#f09bb8` on transparent floating pod contours.
- **Internal Padding:** `62px` fixed-height single-line headers (`16px` horizontal padding); expanded panels add `16px` padding below a `1px` top border.

### Inputs / Fields
- **Style:** Standard form `Draft` inputs use `#fffdfd` fill, `2px` `#f3c4d5` border (`#cf2e68` on focus), `10px` radius; the pod's capsule composer embeds `Draft` in `borderless: true` pill mode inside the `44px` capsule shell.
- **Focus:** `2px` `#cf2e68` border shift (standard inputs) and `#cf2e68` text caret + `20%` alpha berry selection highlight.
- **Masked / Password:** Toggleable mask state (`•`) paired with adjacent `Icon::Eye` / `Icon::EyeOff` circular toggle button.

### Navigation
- **Style:** `196px` fixed `#fadce6` sidebar with three full-width pill items (`コンパニオン`, `接続`, `この端末`), active item filled `#cf2e68` with `#ffffff` bold text and SVG icon; inactive items transparent with `#3b232c` ink and hover tint.

### Floating Speech Pod
- The speech bubble pairs the draggable `#cf2e68` name badge with asymmetric `24/24/8/24px` corners and a `12px` circular tail. The bottom composer pairs a borderless `Draft` with send, and the menu button sits immediately to its right.

## Do's and Don'ts

### Do:
- **Do** use only embedded vector SVG icons from `apps/ene-desktop/src/ui/icons.rs` (`Icon`) for every icon slot.
- **Do** pair every color-coded badge or state indicator with an explicit standard Japanese state word (`接続中`, `未確認`, `接続エラー`, `許可`, `常に許可`, `拒否`).
- **Do** isolate at most one expanded list row (`expanded_companion_id` / `expanded_connection_id`) at a time and separate details via a single `1px` `#f3c4d5` top hairline.
- **Do** keep `float-cutout` shadow strictly on `CompanionPod` floating islands over the transparent `PopUp` window.

### Don't:
- **Don't** render Unicode emojis or text symbols (`✓`, `✕`, `●`, `▸`, `✨`) as UI icons.
- **Don't** place kickers, eyebrows, or explanatory prose paragraphs above headings in `SettingsStudio`.
- **Don't** nest bordered cards inside other bordered cards or apply drop shadows inside `SettingsStudio`.
- **Don't** expose internal host tables (memory lists, work sessions, raw RMS telemetry floats, or avatar test buttons) in user-facing desktop UI.
