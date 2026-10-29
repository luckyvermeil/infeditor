# Heading Level 1

## Heading Level 2

### Heading Level 3

#### Heading Level 4

##### Heading Level 5

###### Heading Level 6

## Inline Formatting

Regular paragraph: **bold**, *italic*, ~~strikethrough~~, `inline code`, [link](https://example.com), ![sample image](./pic.png), inline formulas $E=mc^2$ and $\frac{a}{b}$.
The second line ends with two spaces to force a hard break  
Text after the hard break.

## Blockquote

> A quoted paragraph containing **bold** and `code`.
>
> - First item in the quote
> - Second item in the quote

> > One level of nested quote.

## Lists

- First unordered item
- Second unordered item
  - Nested item a
  - Nested item b
- Third unordered item with `inline code`

### Ordered List

1. First ordered item
2. Second ordered item
3. Third ordered item

### Task List

- [ ] Unchecked task
- [x] Completed task
  - [ ] Nested task (unchecked)
  - [x] Nested task (checked)

## Code Block

```rust
fn main() {
    let s = "| a table | with `backticks` |";
    println!("{s}");
}
```

## Table

| Left | Center | Right |
|:--|:-:|--:|
| **bold** | `code` | [link](https://example.com) |
| inline formula $a^2+b^2=c^2$ | second line<br>break | 3 |

## Horizontal Rule and Display Formulas

---

$$
\int_0^1 x^2 \, dx = \frac{1}{3}
$$

$$
\begin{aligned}
a &= b + c \\
d &= e - f
\end{aligned}
$$

## HTML Block

<div class="note">
The HTML block is preserved verbatim.
</div>

## Escaped Characters

Final paragraph containing escaped characters \*this is not italic\* and an & ampersand.
