//! The protocol's schema as data: every type a frame carries, its fields in order (with the
//! version each came in) or its variants by index. The TypeScript codec is generated from it
//! (`crate::ts`), the docs' frame table is held to it, and its hash goes in `Open` so two builds
//! that disagree on a layout find out at once.

use std::collections::BTreeMap;

/// A field's or a variant field's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    U8,
    U16,
    U32,
    U53,
    I53,
    Bits,
    Bool,
    Str,
    Bytes,
    Opt(Box<Ty>),
    Vec(Box<Ty>),
    /// A struct or enum of the schema, by name.
    Named(&'static str),
}

impl Ty {
    /// As the docs write it: `u53`, `opt<Cursor>` ...
    pub fn render(&self) -> String {
        match self {
            Ty::U8 => "u8".into(),
            Ty::U16 => "u16".into(),
            Ty::U32 => "u32".into(),
            Ty::U53 => "u53".into(),
            Ty::I53 => "i53".into(),
            Ty::Bits => "bits".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Opt(t) => format!("opt<{}>", t.render()),
            Ty::Vec(t) => format!("vec<{}>", t.render()),
            Ty::Named(n) => (*n).to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    /// The protocol version the field came in: a peer speaking an older one isn't sent it.
    pub since: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub name: &'static str,
    pub index: u32,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Struct { name: &'static str, fields: Vec<Field> },
    Enum { name: &'static str, variants: Vec<Variant> },
}

impl Shape {
    pub fn name(&self) -> &'static str {
        match self {
            Shape::Struct { name, .. } | Shape::Enum { name, .. } => name,
        }
    }

    /// Its fields as the docs write them: `id: u53, call: Call`.
    pub fn render_fields(fields: &[Field]) -> String {
        fields.iter().map(|f| format!("{}: {}", f.name, f.ty.render())).collect::<Vec<_>>().join(", ")
    }

    /// One line of the canonical text the schema hash is taken of.
    fn canonical(&self) -> String {
        match self {
            Shape::Struct { name, fields } => {
                let fs: Vec<String> = fields.iter().map(|f| format!("{}:{}@{}", f.name, f.ty.render(), f.since)).collect();
                format!("struct {name}{{{}}}", fs.join(","))
            }
            Shape::Enum { name, variants } => {
                let vs: Vec<String> = variants
                    .iter()
                    .map(|v| {
                        let fs: Vec<String> = v.fields.iter().map(|f| format!("{}:{}", f.name, f.ty.render())).collect();
                        format!("{}={}{{{}}}", v.name, v.index, fs.join(","))
                    })
                    .collect();
                format!("enum {name}{{{}}}", vs.join(","))
            }
        }
    }
}

/// Every shape, by name.
#[derive(Debug, Default, Clone)]
pub struct Registry {
    pub shapes: BTreeMap<&'static str, Shape>,
}

impl Registry {
    /// Adds a shape; false when it was there already (so its contents aren't visited again).
    pub fn add(&mut self, s: Shape) -> bool {
        if self.shapes.contains_key(s.name()) {
            return false;
        }
        self.shapes.insert(s.name(), s);
        true
    }

    pub fn get(&self, name: &str) -> Option<&Shape> {
        self.shapes.get(name)
    }

    /// The canonical text: one line a shape, by name, then the frame kinds (`kinds`: name and
    /// index, in index order).
    pub fn canonical(&self, kinds: &[(&str, u32, u32)]) -> String {
        let mut s = String::new();
        for shape in self.shapes.values() {
            s.push_str(&shape.canonical());
            s.push('\n');
        }
        for (name, index, since) in kinds {
            s.push_str(&format!("kind {name}={index}@{since}\n"));
        }
        s
    }
}

/// FNV-1a, 64 bits: the schema's hash.
pub fn fnv64(b: &[u8]) -> u64 {
    b.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, x| (h ^ u64::from(*x)).wrapping_mul(0x0100_0000_01B3))
}
