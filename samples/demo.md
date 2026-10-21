# fastmark demo

A Markdown file exercising the viewer: **bold**, *italic*, `inline code`,
~~strikethrough~~, [links](https://commonmark.org), and unicode — naïve café 中文 🦀.

## Text

Markdown is a lightweight markup language with plain-text formatting syntax.
fastmark renders it with a streaming parser into a flat block model, lays out
only what is visible, and keeps the source memory-mapped, so even very large
documents scroll smoothly and stay small in RAM.

Soft breaks
flow like this, and a hard break
forces a new line.

## Code

```rust
fn main() {
    let blocks = fastmark_core::parse("# hello");
    println!("{} blocks", blocks.blocks.len());
}
```

Indent code works too:

    indented code block
    second line

## Lists

1. ordered item
2. another
   1. nested ordered
   2. deep nesting
- bullet
- [ ] task open
- [x] task done

## Quotes

> Block quote, first level.
> More text.
>> Nested quote.

## Tables

| Feature | Electron | fastmark |
|---------|:--------:|-------:|
| Idle RAM | ~300 MB | ~50 MB |
| Binary  | ~100 MB | ~10 MB |
| Parser  | DOM     | stream |

## Rules and images

---

Rule above. Images decode off-thread and downscale to display size:

![fastmark logo placeholder](logo.png)

*Inline image note:* if the file is missing you will see a placeholder
frame instead — never a crash.

## Special cases

HTML blocks:

<div align="center">raw html block</div>

Entities: &amp; &lt; &gt; &copy; — footnotes[^1] render inline.

[^1]: Footnote definitions appear as text.

Long line to observe soft wrapping behavior of the viewer when the column is narrow: the text should wrap at word boundaries and stay inside the content column with comfortable measure, not stretch the window.
