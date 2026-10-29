//! Math rendering diagnostic probe (not part of the build;
//! `cargo run -p wysiwyg --example math_probe`): feeds each LaTeX formula
//! extracted from the two sample documents through latex-rust one by one,
//! printing OK/FAIL — used to delimit gaps when extending macro support.
//!
//! **Note**: this probe calls `latex_to_svg` directly, **bypassing** the
//! non-standard macro normalization in `math.rs::normalize_tex` — macros
//! rescued by aliases such as `\lt` will show FAIL here. To verify end-to-end
//! rendering (aliases included) go through `MathSpec::svg()`, see the
//! `math.rs` unit tests.
use latex_rust::{latex_to_svg, Color, Dim, MathFont, SvgOptions};

fn main() {
    let font = MathFont::stix_two_math().expect("font");
    // ---- File 1: Shanghai University of Finance and Economics 2019 mathematical analysis ($..$ / $$..$$) ----
    let exam: &[&str] = &[
        r"x_n \geq 0, y_n \geq 0, n=1,2,3, \ldots",
        r"\left\{\begin{array}{l}x_n^{\frac{4 n+5}{n}}+3 x_n+y_n=1+\frac{1}{n} \\ y_n^{\frac{n+2}{n}}+4 x_n+3 y_n=4-\frac{1}{n}\end{array}\right.",
        r"\left\{x_n\right\},\left\{y_n\right\}",
        r"f^{\prime}(x)",
        r"\int_a^b f^{\prime}(x) d x=f(b)-f(a)",
        r"f\left(\frac{1}{2}\right)=\frac{1}{e}",
        r"\int_L\left[f^{\prime}(x)+6 f(x)\right] y d x+f^{\prime}(x) d y",
        r"x^2+y^2 \leq 1",
        r"z=\left(x^2+y^2\right) \frac{\partial z}{\partial x}+2018\left(\frac{\partial z}{\partial y}\right)^2",
        r"\sum_{n=0}^{\infty} \frac{x^{2 n+1}}{(2 n+1) !!}",
        r"\lim _{x \rightarrow+\infty} e^{-\frac{x^2}{2}} S(x)=\sqrt{\frac{\pi}{2}}",
        r"0\lt f(b)-f(a)\leq \frac{1}{4}(b-a)",
        r"f(x)=|x|, x \in[-\pi, \pi]",
        r"\sum_{n=1}^{\infty} \frac{1}{n^2}",
        r"\int_0^1 \frac{\ln (1-x)}{x} d x",
        r"f(x)=\int_0^x\left(t-t^2\right) \sin ^{2 n} t d t",
        r"f(x) \leq \frac{1}{(2 n+2)(2 n+3)}",
        r"f(z)=2 \iint_{x^2+y^2 \leq z^2}\left(x^2+y^2\right) f\left(\sqrt{x^2+y^2}\right) d x d y+z^4, z>0",
        r"\int_0^1 x f(x) d x \geq \frac{1}{2} \int_0^1 f(x) d x",
        r"f(x)=\int_x^{x^2}\left(1+\frac{2}{t}\right)^t \sin \frac{1}{\sqrt{t}} d t, x>1",
        r"\sum_{n=2}^{\infty} \frac{1}{f(n)}",
    ];
    // ---- File 2: "Why is the Earth's silicon content so large" (\mathrm chemical formulas inside \\(..\\)) ----
    let geo: &[&str] = &[
        r"\mathrm{SiO_4}",
        r"\mathrm{SiO_2}",
        r"\mathrm{(Mg,Fe)\_2SiO_4}",
        // Variant probe: escaped underscore vs proper subscript
        r"\mathrm{(Mg,Fe)_2SiO_4}",
    ];
    // ---- File 3: boxed gathered flowchart (CJK \text + \downarrow) ----
    // Probe results as of 2026-10-23: \downarrow/\boxed/\text{Latin} are
    // native; `gathered` and CJK-in-\text FAIL on the raw engine — both are
    // rescued at the math.rs layer (gathered → \begin{array}{c} rewrite;
    // CJK via the vendor patch's fallback face), so these cases document the
    // engine's floor, not the end-user behavior. End-to-end coverage lives in
    // math.rs tests (boxed_gathered_cjk_flowchart_renders et al).
    let flow: &[&str] = &[
        r"\downarrow",
        r"\text{测量碱性岩}",
        r"\text{Hello}",
        r"\boxed{\text{你好}}",
        r"\begin{gathered}\text{甲}\\ \text{乙}\end{gathered}",
        r"\boxed{\begin{aligned}\text{甲}\\ \downarrow\\ \text{乙}\end{aligned}}",
        r"\boxed{\begin{array}{c}\text{甲}\\ \downarrow\\ \text{乙}\end{array}}",
    ];
    let mut fail = 0;
    for (name, list) in [("exam", exam), ("geo", geo), ("flow", flow)] {
        for src in list {
            let mut opts = SvgOptions::new();
            opts.font_size_pt = Dim::ratio(16, 1);
            opts.color = Color::rgb(0x33, 0x33, 0x33);
            match latex_to_svg(src, &font, &opts) {
                Ok(svg) => println!("[{name}] OK   ({:5} B) {}", svg.len(), short(src)),
                Err(e) => {
                    fail += 1;
                    println!("[{name}] FAIL {:<28} {}", format!("{e}"), short(src));
                }
            }
        }
    }
    // Dump the aligned-rewrite SVG so CJK glyph presence can be inspected
    // offline (tmp/): path count is a cheap proxy for "text actually drew".
    {
        let mut opts = SvgOptions::new();
        opts.font_size_pt = Dim::ratio(16, 1);
        opts.color = Color::rgb(0x33, 0x33, 0x33);
        if let Ok(svg) = latex_to_svg(
            r"\boxed{\begin{aligned}\text{甲}\\ \downarrow\\ \text{乙}\end{aligned}}",
            &font,
            &opts,
        ) {
            let _ = std::fs::write(r"tmp\flow_probe.svg", &svg);
            println!(
                "flow svg written: {} B, {} paths",
                svg.len(),
                svg.matches("<path").count()
            );
        }
    }
    // Dump the full user flowchart through the END-TO-END path (MathSpec:
    // normalize_tex alias/env rewrites + CJK fallback face), so the real
    // rendering can be inspected offline (tmp/).
    {
        const SRC: &str = r"\boxed{ \begin{gathered} \text{测量碱性岩的 F、Cl、C、S}\\ \downarrow\\ \text{评估蚀变、脱气及岩浆分异的影响}\\ \downarrow\\ \text{利用 F/Cl、F/Nd、Cl/Nb 追踪卤素行为}\\ \downarrow\\ \text{结合 Th/Yb、Ba/La 判断交代来源}\\ \downarrow\\ \text{用非模态部分熔融模型反推地幔源区}\\ \downarrow\\ \text{区分沉积物熔融与板片脱水的作用}\\ \downarrow\\ \text{讨论卤素向深部地幔的再循环} \end{gathered} }";
        match wysiwyg::math::MathSpec::block(SRC, "#333333", 16.0).svg() {
            Some(svg) => {
                let _ = std::fs::write(r"tmp\flow_e2e.svg", &svg);
                println!(
                    "flow e2e: OK ({} B, {} paths) -> tmp\\flow_e2e.svg",
                    svg.len(),
                    svg.matches("<path").count()
                );
            }
            None => println!("flow e2e: FAIL (fell back to text)"),
        }
    }
    println!("---- {fail} failures ----");
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(72).collect();
    if s.chars().count() > 72 {
        format!("{t}…")
    } else {
        t
    }
}
