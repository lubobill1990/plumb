/**
 * UI icons as inline SVG (24x24, stroked with currentColor). They replace the Segoe
 * Fluent Icons font glyphs, which only exist on Windows. Keyed by the old codepoints so
 * the call sites read the same.
 */
const PATHS: Record<string, string> = {
  "\uE710": "M12 5v14M5 12h14", // plus
  "\uE70D": "M6 9l6 6 6-6", // chevron down
  "\uE70E": "M6 15l6-6 6 6", // chevron up
  "\uE76B": "M15 6l-6 6 6 6", // chevron left
  "\uE76C": "M9 6l6 6-6 6", // chevron right
  "\uE711": "M6 6l12 12M18 6L6 18", // close
  "\uE8BB": "M6 6l12 12M18 6L6 18", // close (window)
  "\uE921": "M5 12h14", // minimize
  "\uE922": "M5.5 5.5h13v13h-13z", // maximize
  "\uE923": "M8.5 8.5V5.5h10v10h-3M5.5 8.5h10v10h-10z", // restore
  // palette (tab color)
  "\uE790":
    "M12 3.5a8.5 8.5 0 1 0 0 17c1 0 1.7-.8 1.7-1.7 0-.5-.2-.9-.5-1.2-.3-.3-.5-.7-.5-1.2 0-1 .8-1.7 1.7-1.7h2.1a4 4 0 0 0 4-4C20.5 6.8 16.7 3.5 12 3.5zM7.5 12h.01M9.5 8h.01M14.5 8h.01",
  "\uE8AC": "M4 20h4L18.5 9.5l-4-4L4 16v4zM13 7l4 4", // rename (pencil)
  "\uE8C8": "M9 9h11v11H9zM15 9V4H4v11h5", // duplicate
  "\uE8A7": "M14 4h6v6M20 4l-9 9M18 14v6H4V6h6", // open in new window
  "\uE74E": "M5 4h11l3 3v13H5zM8 4v5h7V4M8 20v-6h8v6", // save / export
  "\uE721": "M10.5 4.5a6 6 0 1 0 0 12 6 6 0 0 0 0-12zM15 15l5 5", // search
  "\uE895": "M20 12a8 8 0 1 1-2.3-5.6M20 4v5h-5", // check for updates
  "\uE713": "M4 7h9M17 7h3M4 17h3M11 17h9M15 5v4M9 15v4", // auto-check switch (sliders)
  "\uE7F4": "M3.5 5h17v11h-17zM9 20h6M12 16v4", // renderer (display)
  "\uE7A7": "M9 13L4 8l5-5M4 8h10a6 6 0 0 1 0 12h-3", // roll back
  "\uE8A5": "M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 16h6", // log
};

export function svgIcon(codepoint: string, size = 14): string {
  const d = PATHS[codepoint];
  if (!d) return "";
  return `<svg class="ico" viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${d}"/></svg>`;
}

/** A menu / button glyph element. */
export function glyph(codepoint: string, cls = "glyph"): HTMLSpanElement {
  const span = document.createElement("span");
  span.className = cls;
  span.innerHTML = svgIcon(codepoint);
  return span;
}
