# MuPDF fork

This is `mupdf-sys` 0.8.0 (MuPDF 1.27.2) with a small local patch for
reflowable documents:

- `mupdf/source/html/html-layout.c`: measure the available reflow page height
  from the page box, not the (possibly content-inflated) block bounds, with a
  fallback for the zero-height reflow layout. This shrinks oversized images to
  one page instead of splitting them across pages.
- `mupdf/source/reflow/reflow-doc.c`: pass the document page height into the
  HTML layout call used for reflowed pages.

The rest of the crate is unmodified.
