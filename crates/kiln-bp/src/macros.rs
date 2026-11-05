//! The macros the protocol's types are declared with. Each declares a type once and makes from it
//! everything the protocol needs of it: its encoding, its tolerant reading as a frame body, its
//! shape in the schema (which the TypeScript codec is generated from), its JSON for the shared
//! vectors and its structure-aware fuzz draw. So no type has a second, hand-kept copy anywhere.

/// A field's version: 1 unless it says `= since N`.
#[doc(hidden)]
#[macro_export]
macro_rules! bp_since {
    () => {
        1u32
    };
    ($n:literal) => {
        $n
    };
}

/// A struct of the protocol: fields in order, each `pub name: Type`, a field added after the
/// first version marked `= since N` (and only ever after every older field).
#[macro_export]
macro_rules! bp_struct {
    (
        $(#[$m:meta])*
        pub struct $name:ident {
            $( $(#[$fm:meta])* pub $f:ident : $t:ty $(= since $since:literal)? ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Default)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $t, )+
        }

        impl $crate::wire::Wire for $name {
            fn put(&self, w: &mut $crate::wire::Writer) {
                $( $crate::wire::Wire::put(&self.$f, w); )+
            }
            fn take(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<Self, $crate::wire::Refusal> {
                ::core::result::Result::Ok(Self { $( $f: <$t as $crate::wire::Wire>::take(r)?, )+ })
            }
            fn skip(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<(), $crate::wire::Refusal> {
                $( <$t as $crate::wire::Wire>::skip(r)?; )+
                ::core::result::Result::Ok(())
            }
            fn ty() -> $crate::schema::Ty {
                $crate::schema::Ty::Named(stringify!($name))
            }
            fn visit(reg: &mut $crate::schema::Registry) {
                let shape = $crate::schema::Shape::Struct {
                    name: stringify!($name),
                    fields: vec![ $( $crate::schema::Field { name: stringify!($f), ty: <$t as $crate::wire::Wire>::ty(), since: $crate::bp_since!($($since)?) }, )+ ],
                };
                if reg.add(shape) {
                    $( <$t as $crate::wire::Wire>::visit(reg); )+
                }
            }
            #[allow(unused_assignments)]
            fn json(&self, out: &mut String) {
                out.push('{');
                let mut sep = "";
                $(
                    out.push_str(sep);
                    sep = ",";
                    out.push_str(concat!("\"", stringify!($f), "\":"));
                    $crate::wire::Wire::json(&self.$f, out);
                )+
                out.push('}');
            }
            fn arb(g: &mut $crate::rng::Rng) -> Self {
                Self { $( $f: <$t as $crate::wire::Wire>::arb(g), )+ }
            }
        }

        impl $crate::wire::Fields for $name {
            fn fields(&self) -> ::std::vec::Vec<$crate::wire::Bytes> {
                vec![ $( $crate::wire::Bytes($crate::wire::to_bytes(&self.$f)), )+ ]
            }
            fn from_fields(fields: &[$crate::wire::Bytes]) -> ::core::result::Result<Self, $crate::wire::Refusal> {
                let mut it = fields.iter();
                ::core::result::Result::Ok(Self { $( $f: match it.next() {
                    ::core::option::Option::Some(b) if !b.0.is_empty() => $crate::wire::from_bytes::<$t>(&b.0)?,
                    _ => ::core::default::Default::default(),
                }, )+ })
            }
        }

        impl $crate::wire::Body for $name {
            fn put_body(&self, w: &mut $crate::wire::Writer, version: u32) {
                $(
                    if $crate::bp_since!($($since)?) > version {
                        return;
                    }
                    $crate::wire::Wire::put(&self.$f, w);
                )+
            }
            fn take_body(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<Self, $crate::wire::Refusal> {
                let mut v = Self::default();
                $(
                    if r.is_empty() {
                        return ::core::result::Result::Ok(v);
                    }
                    v.$f = <$t as $crate::wire::Wire>::take(r)?;
                )+
                ::core::result::Result::Ok(v)
            }
            fn skip_body(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<(), $crate::wire::Refusal> {
                $(
                    if r.is_empty() {
                        return ::core::result::Result::Ok(());
                    }
                    <$t as $crate::wire::Wire>::skip(r)?;
                )+
                ::core::result::Result::Ok(())
            }
        }
    };
}

/// An enum of the protocol: each variant unit-like or with named fields, and its index on the
/// wire (`= N`), which never changes; new variants go after every one there. The first is the
/// default a missing trailing field reads as.
#[macro_export]
macro_rules! bp_enum {
    (
        $(#[$m:meta])*
        pub enum $name:ident {
            $(#[$v0m:meta])* $v0:ident $( { $( $(#[$f0m:meta])* $f0:ident : $t0:ty ),* $(,)? } )? = $i0:literal
            $( , $(#[$vm:meta])* $v:ident $( { $( $(#[$fm:meta])* $f:ident : $t:ty ),* $(,)? } )? = $i:literal )* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $(#[$v0m])* $v0 $( { $( $(#[$f0m])* $f0: $t0 ),* } )?,
            $( $(#[$vm])* $v $( { $( $(#[$fm])* $f: $t ),* } )?, )*
        }

        impl ::core::default::Default for $name {
            fn default() -> Self {
                Self::$v0 $( { $( $f0: ::core::default::Default::default() ),* } )?
            }
        }

        $crate::bp_enum!(@impl $name { $v0 $( { $( $f0 : $t0 ),* } )? = $i0 $( , $v $( { $( $f : $t ),* } )? = $i )* });
    };

    (@impl $name:ident { $( $v:ident $( { $( $f:ident : $t:ty ),* } )? = $i:literal ),+ }) => {
        impl $name {
            /// Its variants' names, in index order.
            pub const VARIANTS: &'static [&'static str] = &[ $( stringify!($v) ),+ ];

            /// Its variant's index on the wire.
            pub fn variant(&self) -> u32 {
                match self {
                    $( Self::$v $( { $( $f: _ ),* } )? => $i, )+
                }
            }
        }

        impl $crate::wire::Wire for $name {
            #[allow(unused_variables)]
            fn put(&self, w: &mut $crate::wire::Writer) {
                match self {
                    $(
                        Self::$v $( { $( $f ),* } )? => {
                            w.varint($i);
                            $( $( $crate::wire::Wire::put($f, w); )* )?
                        }
                    )+
                }
            }
            fn take(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<Self, $crate::wire::Refusal> {
                match r.varint()? {
                    $( $i => ::core::result::Result::Ok(Self::$v $( { $( $f: <$t as $crate::wire::Wire>::take(r)? ),* } )?), )+
                    _ => ::core::result::Result::Err($crate::wire::Why::UnknownVariant.into()),
                }
            }
            fn skip(r: &mut $crate::wire::Reader<'_>) -> ::core::result::Result<(), $crate::wire::Refusal> {
                match r.varint()? {
                    $( $i => { $( $( <$t as $crate::wire::Wire>::skip(r)?; )* )? ::core::result::Result::Ok(()) } )+
                    _ => ::core::result::Result::Err($crate::wire::Why::UnknownVariant.into()),
                }
            }
            fn ty() -> $crate::schema::Ty {
                $crate::schema::Ty::Named(stringify!($name))
            }
            fn visit(reg: &mut $crate::schema::Registry) {
                let shape = $crate::schema::Shape::Enum {
                    name: stringify!($name),
                    variants: vec![ $( $crate::schema::Variant {
                        name: stringify!($v),
                        index: $i,
                        fields: vec![ $( $( $crate::schema::Field { name: stringify!($f), ty: <$t as $crate::wire::Wire>::ty(), since: 1 } ),* )? ],
                    }, )+ ],
                };
                if reg.add(shape) {
                    $( $( $( <$t as $crate::wire::Wire>::visit(reg); )* )? )+
                }
            }
            #[allow(unused_variables)]
            fn json(&self, out: &mut String) {
                match self {
                    $(
                        Self::$v $( { $( $f ),* } )? => {
                            out.push_str(concat!("{\"t\":\"", stringify!($v), "\""));
                            $( $(
                                out.push_str(concat!(",\"", stringify!($f), "\":"));
                                $crate::wire::Wire::json($f, out);
                            )* )?
                            out.push('}');
                        }
                    )+
                }
            }
            fn arb(g: &mut $crate::rng::Rng) -> Self {
                let n = Self::VARIANTS.len();
                let pick = g.index(n);
                let mut at = 0usize;
                $(
                    if pick == at {
                        return Self::$v $( { $( $f: <$t as $crate::wire::Wire>::arb(g) ),* } )?;
                    }
                    at = at.saturating_add(1);
                )+
                let _ = at;
                Self::default()
            }
        }
    };
}
