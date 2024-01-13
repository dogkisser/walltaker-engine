use std::{fs, io::Write};

fn main() {
    println!("cargo:rerun-if-changed=res/walltaker-engine.rc");
    embed_resource::compile("res/walltaker-engine.rc", embed_resource::NONE);

    use std::env;
    let out_dir = env::var("OUT_DIR").unwrap();

    minify("res/background.html", &format!("{out_dir}/background.html.min"));
    minify("res/settings.html", &format!("{out_dir}/settings.html.min"));
}

fn minify(from: &str, to: &str) {
    println!("cargo:rerun-if-changed={from}");
    let mut minify_cfg = minify_html::Cfg::new();
    minify_cfg.minify_js = true;
    minify_cfg.minify_css = true;

    let inf = fs::read_to_string(from).unwrap();
    let mut outf = fs::File::create(to).unwrap();
    outf.write_all(&minify_html::minify(inf.as_bytes(), &minify_cfg)).unwrap();
}