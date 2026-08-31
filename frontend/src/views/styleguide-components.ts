/**
 * The UI primitives the living styleguide renders.
 *
 * Kept beside the view so the unit test can compare this explicit inventory to
 * `components/ui/` without importing a browser-only React tree.
 */
export const STYLEGUIDE_COMPONENTS = [
  "alert",
  "alert-dialog",
  "avatar",
  "badge",
  "button",
  "card",
  "chart",
  "dialog",
  "dropdown-menu",
  "input",
  "label",
  "popover",
  "scroll-area",
  "select",
  "separator",
  "sheet",
  "sidebar",
  "skeleton",
  "sonner",
  "stepper",
  "switch",
  "tabs",
  "textarea",
  "tooltip",
] as const;
