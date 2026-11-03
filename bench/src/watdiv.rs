//! The WatDiv-like workload: a deterministic e-commerce / social data
//! generator in the spirit of the Waterloo SPARQL Diversity Test Suite
//! (WatDiv, Aluç et al., ISWC 2014) and its 20 basic query templates
//! (`bench/queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq`: linear, star, snowflake
//! and complex shapes).
//!
//! The schema is a simplified version of WatDiv's: the same prefixes
//! (`wsdbm:`, `sorg:`, `gr:`, `rev:`, `foaf:`, `og:`, `mo:`, `gn:`, `dc:`) and
//! entity classes, and every predicate the 20 templates use. Scalable classes
//! grow with the scale factor (`User` 1000·s, `Product` 250·s, `Review`
//! 1500·s, `Offer` 900·s, `Purchase` 1500·s, `Retailer` 12·s, `Website`
//! 50·s); the dictionary-like classes (`City` 240, `Country` 25, `Topic` 250,
//! `SubGenre` 145, …) keep their size. Social edges (`wsdbm:friendOf`,
//! `wsdbm:follows`) and most references pick their target from a Zipf
//! distribution, so a few users and products are very popular (`Product0`
//! is the most liked product); about 100K triples per scale unit, 70% of
//! them social edges as in WatDiv.
//!
//! The generator writes every triple of a subject together and never writes
//! a line twice, so its lines are its unique triples. The fixed constants of
//! the templates (`Product0`, `Role2`, `Country1`, `Country5`, `Language0`,
//! `ProductCategory2`) always exist, and [`GUARANTEED`] gives entity 0 of
//! each class the triples that make every template answerable with the
//! placeholder bound to that entity (see [`instantiate`](WatDiv::instantiate)).
//!
//! Randomness: one [`SplitMix64`] stream per class (its entities in order),
//! one per owned relation (which parent owns each child) and one per
//! template (its instantiation), each seeded from (seed, tag, index) by
//! [`stream_seed`]. Zipf weights are integers (`2^40 / k` and
//! `2^48 / isqrt(k·2^32)`), so the output is the same on every platform:
//! a pure function of ([`WatDiv::scale`], [`WatDiv::seed`],
//! [`WatDiv::shrink`]) and of [`VERSION`]. `bench/tests/watdiv.rs` pins the
//! hash of scale 1, seed 0.
//!
//! **Queries** are templates with `%vN%` placeholders and a `#mapping vN
//! wsdbm:Class uniform` header (`#mapping none` for the constant-free C1–C3).
//! They are instantiated against the generated data by an **ASK walk**: for
//! each template, up to [`MAX_TRIES`] candidate bindings are drawn from the
//! template's own stream (uniform over the class's entities; the last try is
//! entity 0, which [`GUARANTEED`] makes answerable) and the first one whose
//! `ASK` is true wins. The walk is deterministic (the stream and the data
//! fix the candidates, the engine's answers fix the winner), so the bound
//! queries are a function of the workload parameters; the runner caches them
//! next to the data file. A template that no candidate answers fails the run.
//!
//! The update chunk is [`CHUNK_USERS`] users beyond the data set's last one
//! (`User{n}` …), with the user properties and references into the data:
//! every chunk triple has a new subject, so the chunk is disjoint from the
//! data.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

use crate::rng::{stream_seed, SplitMix64};
use crate::workload::{BoundQuery, GenStats, QuerySpec, Workload};

/// The generator version; part of every baseline's exact gates. Bump it on
/// any change to the output (data, templates or the ASK walk).
pub const VERSION: &str = "watdiv-like/1";

/// Candidate bindings an ASK walk tries per template.
pub const MAX_TRIES: u32 = 32;

/// Users of the update chunk (divided by `shrink`, at least 1).
pub const CHUNK_USERS: u64 = 100;

/// The namespace of WatDiv's own vocabulary and entities.
pub const WSDBM: &str = "http://db.uwaterloo.ca/~galuc/wsdbm/";

const PREFIXES: [(&str, &str); 12] = [
    ("wsdbm", WSDBM),
    ("sorg", "http://schema.org/"),
    ("dc", "http://purl.org/dc/terms/"),
    ("foaf", "http://xmlns.com/foaf/"),
    ("gr", "http://purl.org/goodrelations/"),
    ("gn", "http://www.geonames.org/ontology#"),
    ("mo", "http://purl.org/ontology/mo/"),
    ("og", "http://ogp.me/ns#"),
    ("rev", "http://purl.org/stuff/rev#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

/// The 20 templates, verbatim from `bench/queries/watdiv/`.
pub const TEMPLATES: [(&str, &str); 20] = [
    ("L1", include_str!("../queries/watdiv/L1.rq")),
    ("L2", include_str!("../queries/watdiv/L2.rq")),
    ("L3", include_str!("../queries/watdiv/L3.rq")),
    ("L4", include_str!("../queries/watdiv/L4.rq")),
    ("L5", include_str!("../queries/watdiv/L5.rq")),
    ("S1", include_str!("../queries/watdiv/S1.rq")),
    ("S2", include_str!("../queries/watdiv/S2.rq")),
    ("S3", include_str!("../queries/watdiv/S3.rq")),
    ("S4", include_str!("../queries/watdiv/S4.rq")),
    ("S5", include_str!("../queries/watdiv/S5.rq")),
    ("S6", include_str!("../queries/watdiv/S6.rq")),
    ("S7", include_str!("../queries/watdiv/S7.rq")),
    ("F1", include_str!("../queries/watdiv/F1.rq")),
    ("F2", include_str!("../queries/watdiv/F2.rq")),
    ("F3", include_str!("../queries/watdiv/F3.rq")),
    ("F4", include_str!("../queries/watdiv/F4.rq")),
    ("F5", include_str!("../queries/watdiv/F5.rq")),
    ("C1", include_str!("../queries/watdiv/C1.rq")),
    ("C2", include_str!("../queries/watdiv/C2.rq")),
    ("C3", include_str!("../queries/watdiv/C3.rq")),
];

/// Expand a prefixed name (`wsdbm:User0`) to an N-Triples IRI, and a
/// literal's prefixed datatype (`"7"^^xsd:integer`) to its IRI; anything
/// else is returned as it is.
pub fn expand(term: &str) -> String {
    if term.starts_with('"') {
        if let Some((lit, dt)) = term.rsplit_once("^^") {
            if !dt.starts_with('<') {
                return format!("{lit}^^{}", expand(dt));
            }
        }
        return term.to_string();
    }
    if let Some((p, local)) = term.split_once(':') {
        if let Some((_, ns)) = PREFIXES.iter().find(|(q, _)| *q == p) {
            return format!("<{ns}{local}>");
        }
    }
    term.to_string()
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// An entity class: its name (the IRI local-name prefix), its size at scale 1
/// and whether it grows with the scale factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Class {
    pub name: &'static str,
    pub base: u64,
    pub scalable: bool,
}

pub const USER: usize = 0;
pub const PRODUCT: usize = 1;
pub const REVIEW: usize = 2;
pub const OFFER: usize = 3;
pub const PURCHASE: usize = 4;
pub const RETAILER: usize = 5;
pub const WEBSITE: usize = 6;
pub const CITY: usize = 7;
pub const COUNTRY: usize = 8;
pub const LANGUAGE: usize = 9;
pub const TOPIC: usize = 10;
pub const GENRE: usize = 11;
pub const SUBGENRE: usize = 12;
pub const PRODUCT_CATEGORY: usize = 13;
pub const ROLE: usize = 14;
pub const AGE_GROUP: usize = 15;
pub const GENDER: usize = 16;

/// Every class, in generation order (indexed by the constants above).
pub const CLASSES: [Class; 17] = [
    class("User", 1000, true),
    class("Product", 250, true),
    class("Review", 1500, true),
    class("Offer", 900, true),
    class("Purchase", 1500, true),
    class("Retailer", 12, true),
    class("Website", 50, true),
    class("City", 240, false),
    class("Country", 25, false),
    class("Language", 25, false),
    class("Topic", 250, false),
    class("Genre", 21, false),
    class("SubGenre", 145, false),
    class("ProductCategory", 15, false),
    class("Role", 3, false),
    class("AgeGroup", 9, false),
    class("Gender", 2, false),
];

const fn class(name: &'static str, base: u64, scalable: bool) -> Class {
    Class {
        name,
        base,
        scalable,
    }
}

/// How a reference picks its target entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Uniform,
    /// Zipf with exponent 1: entity 0 is the most popular.
    Zipf1,
    /// Zipf with exponent 1/2 (a milder skew).
    ZipfHalf,
}

/// The object of a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obj {
    /// An entity of a class.
    Ref(usize, Pick),
    /// A fixed IRI (prefixed name).
    Iri(&'static str),
    /// A plain literal: the prefix and a number below the bound.
    Name(&'static str, u64),
    /// A plain literal of `lo..=hi` words.
    Words(u64, u64),
    /// An `xsd:integer` in `lo..=hi`.
    Int(u64, u64),
    /// A plain `yyyy-mm-dd` literal.
    Date,
    /// A plain `d.cc` literal.
    Price,
    /// The subject's e-mail address.
    Email,
    /// The subject's URL.
    Url,
    /// The subject's local name as a literal.
    Label,
}

/// A property of a class: with probability `permille`/1000 an entity has
/// `n.0..=n.1` distinct objects of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prop {
    pub pred: &'static str,
    pub permille: u64,
    pub n: (u64, u64),
    pub obj: Obj,
}

const fn p(pred: &'static str, permille: u64, n: (u64, u64), obj: Obj) -> Prop {
    Prop {
        pred,
        permille,
        n,
        obj,
    }
}

const ONE: (u64, u64) = (1, 1);

const USER_PROPS: &[Prop] = &[
    p("rdf:type", 500, ONE, Obj::Ref(ROLE, Pick::Uniform)),
    p("foaf:givenName", 900, ONE, Obj::Name("Given", 2000)),
    p("foaf:familyName", 800, ONE, Obj::Name("Family", 5000)),
    p("foaf:age", 500, ONE, Obj::Ref(AGE_GROUP, Pick::Uniform)),
    p("wsdbm:gender", 600, ONE, Obj::Ref(GENDER, Pick::Uniform)),
    p(
        "sorg:nationality",
        300,
        ONE,
        Obj::Ref(COUNTRY, Pick::Uniform),
    ),
    p("dc:Location", 400, ONE, Obj::Ref(CITY, Pick::Uniform)),
    p("sorg:jobTitle", 50, ONE, Obj::Name("Job", 200)),
    p("sorg:email", 900, ONE, Obj::Email),
    p("sorg:telephone", 200, ONE, Obj::Name("555-", 10_000)),
    p("foaf:homepage", 50, ONE, Obj::Ref(WEBSITE, Pick::Uniform)),
    p(
        "wsdbm:subscribes",
        250,
        (1, 3),
        Obj::Ref(WEBSITE, Pick::Zipf1),
    ),
    p("wsdbm:likes", 600, (1, 5), Obj::Ref(PRODUCT, Pick::Zipf1)),
    p(
        "wsdbm:friendOf",
        400,
        (1, 199),
        Obj::Ref(USER, Pick::ZipfHalf),
    ),
    p(
        "wsdbm:follows",
        400,
        (1, 149),
        Obj::Ref(USER, Pick::ZipfHalf),
    ),
];

const PRODUCT_PROPS: &[Prop] = &[
    p(
        "rdf:type",
        1000,
        ONE,
        Obj::Ref(PRODUCT_CATEGORY, Pick::Uniform),
    ),
    p("sorg:caption", 400, ONE, Obj::Words(2, 4)),
    p("sorg:text", 400, ONE, Obj::Words(6, 12)),
    p("sorg:contentRating", 400, ONE, Obj::Name("Rating", 10)),
    p("sorg:contentSize", 400, ONE, Obj::Int(1, 4000)),
    p("sorg:description", 600, ONE, Obj::Words(5, 10)),
    p("sorg:keywords", 500, ONE, Obj::Words(3, 3)),
    p("sorg:language", 500, ONE, Obj::Ref(LANGUAGE, Pick::Zipf1)),
    p("sorg:publisher", 400, ONE, Obj::Name("Publisher", 50)),
    p("og:title", 700, ONE, Obj::Words(1, 3)),
    p("og:tag", 800, (1, 3), Obj::Ref(TOPIC, Pick::Uniform)),
    p(
        "wsdbm:hasGenre",
        1000,
        (1, 3),
        Obj::Ref(SUBGENRE, Pick::Uniform),
    ),
    p("foaf:homepage", 400, ONE, Obj::Ref(WEBSITE, Pick::Uniform)),
    p("sorg:trailer", 200, ONE, Obj::Name("Trailer", 100_000)),
    p("sorg:actor", 300, (1, 5), Obj::Ref(USER, Pick::ZipfHalf)),
    p("mo:artist", 300, (1, 2), Obj::Ref(USER, Pick::ZipfHalf)),
    p("mo:conductor", 100, ONE, Obj::Ref(USER, Pick::ZipfHalf)),
    p("sorg:director", 100, ONE, Obj::Ref(USER, Pick::ZipfHalf)),
    p("sorg:datePublished", 500, ONE, Obj::Date),
];

const REVIEW_PROPS: &[Prop] = &[
    p("rev:title", 600, ONE, Obj::Words(2, 5)),
    p("rev:reviewer", 1000, ONE, Obj::Ref(USER, Pick::Uniform)),
    p("rev:rating", 1000, ONE, Obj::Int(1, 10)),
    p("rev:text", 600, ONE, Obj::Words(8, 16)),
    p("rev:totalVotes", 500, ONE, Obj::Int(0, 500)),
];

const OFFER_PROPS: &[Prop] = &[
    p("gr:includes", 1000, ONE, Obj::Ref(PRODUCT, Pick::ZipfHalf)),
    p("gr:price", 1000, ONE, Obj::Price),
    p("gr:serialNumber", 700, ONE, Obj::Name("SN", 1_000_000_000)),
    p("gr:validFrom", 600, ONE, Obj::Date),
    p("gr:validThrough", 600, ONE, Obj::Date),
    p("sorg:eligibleQuantity", 600, ONE, Obj::Int(1, 100)),
    p(
        "sorg:eligibleRegion",
        700,
        (1, 4),
        Obj::Ref(COUNTRY, Pick::Uniform),
    ),
    p("sorg:priceValidUntil", 500, ONE, Obj::Date),
];

const PURCHASE_PROPS: &[Prop] = &[
    p(
        "wsdbm:purchaseFor",
        1000,
        ONE,
        Obj::Ref(PRODUCT, Pick::Zipf1),
    ),
    p("wsdbm:purchaseDate", 800, ONE, Obj::Date),
    p("gr:price", 700, ONE, Obj::Price),
];

const RETAILER_PROPS: &[Prop] = &[
    p("sorg:legalName", 1000, ONE, Obj::Words(2, 3)),
    p("sorg:paymentAccepted", 500, (1, 2), Obj::Name("Payment", 5)),
];

const WEBSITE_PROPS: &[Prop] = &[
    p("sorg:url", 1000, ONE, Obj::Url),
    p("wsdbm:hits", 1000, ONE, Obj::Int(0, 100_000)),
    p(
        "sorg:language",
        1000,
        (1, 2),
        Obj::Ref(LANGUAGE, Pick::Zipf1),
    ),
];

const CITY_PROPS: &[Prop] = &[p(
    "gn:parentCountry",
    1000,
    ONE,
    Obj::Ref(COUNTRY, Pick::Uniform),
)];

const SUBGENRE_PROPS: &[Prop] = &[
    p("rdf:type", 1000, ONE, Obj::Iri("wsdbm:SubGenre")),
    p("og:tag", 700, (1, 2), Obj::Ref(TOPIC, Pick::Uniform)),
];

const COUNTRY_PROPS: &[Prop] = &[p("rdf:type", 1000, ONE, Obj::Iri("wsdbm:Country"))];
const LANGUAGE_PROPS: &[Prop] = &[p("rdf:type", 1000, ONE, Obj::Iri("wsdbm:Language"))];
const TOPIC_PROPS: &[Prop] = &[p("rdf:type", 1000, ONE, Obj::Iri("wsdbm:Topic"))];
const GENRE_PROPS: &[Prop] = &[p("rdf:type", 1000, ONE, Obj::Iri("wsdbm:Genre"))];
const LABEL_PROPS: &[Prop] = &[p("rdfs:label", 1000, ONE, Obj::Label)];

/// The properties of each class (indexed like [`CLASSES`]).
pub fn props(class: usize) -> &'static [Prop] {
    match class {
        USER => USER_PROPS,
        PRODUCT => PRODUCT_PROPS,
        REVIEW => REVIEW_PROPS,
        OFFER => OFFER_PROPS,
        PURCHASE => PURCHASE_PROPS,
        RETAILER => RETAILER_PROPS,
        WEBSITE => WEBSITE_PROPS,
        CITY => CITY_PROPS,
        COUNTRY => COUNTRY_PROPS,
        LANGUAGE => LANGUAGE_PROPS,
        TOPIC => TOPIC_PROPS,
        GENRE => GENRE_PROPS,
        SUBGENRE => SUBGENRE_PROPS,
        _ => LABEL_PROPS,
    }
}

/// A one-to-many relation in which every child has exactly one parent
/// (`Retailer gr:offers Offer`): the parents are drawn per child, the triples
/// written with the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Own {
    pub parent: usize,
    pub pred: &'static str,
    pub child: usize,
    pub pick: Pick,
}

pub const OWNS: [Own; 3] = [
    Own {
        parent: RETAILER,
        pred: "gr:offers",
        child: OFFER,
        pick: Pick::Uniform,
    },
    Own {
        parent: PRODUCT,
        pred: "rev:hasReview",
        child: REVIEW,
        pick: Pick::ZipfHalf,
    },
    Own {
        parent: USER,
        pred: "wsdbm:makesPurchase",
        child: PURCHASE,
        pick: Pick::Uniform,
    },
];

/// Triples entity 0 (or 1) of a class always has (class, index, predicate,
/// object): with them every template is answerable when its placeholder is
/// bound to entity 0 of the mapped class (the ASK walk's last try), and the
/// templates' fixed constants exist. Child 0 of every [`OWNS`] relation
/// belongs to parent 0 (`Retailer0 gr:offers Offer0`, `Product0
/// rev:hasReview Review0`, `User0 wsdbm:makesPurchase Purchase0`). A
/// guaranteed single-valued property replaces the random one.
pub const GUARANTEED: &[(usize, u64, &str, &str)] = &[
    (USER, 0, "rdf:type", "wsdbm:Role2"),
    (USER, 0, "foaf:givenName", "\"Given0\""),
    (USER, 0, "foaf:familyName", "\"Family0\""),
    (USER, 0, "foaf:age", "wsdbm:AgeGroup0"),
    (USER, 0, "wsdbm:gender", "wsdbm:Gender0"),
    (USER, 0, "sorg:nationality", "wsdbm:Country1"),
    (USER, 0, "dc:Location", "wsdbm:City0"),
    (USER, 0, "sorg:jobTitle", "\"Job0\""),
    (USER, 0, "foaf:homepage", "wsdbm:Website0"),
    (USER, 0, "wsdbm:subscribes", "wsdbm:Website0"),
    (USER, 0, "wsdbm:likes", "wsdbm:Product0"),
    (USER, 0, "wsdbm:friendOf", "wsdbm:User1"),
    (USER, 1, "rdf:type", "wsdbm:Role2"),
    (USER, 1, "sorg:nationality", "wsdbm:Country0"),
    (USER, 1, "dc:Location", "wsdbm:City1"),
    (USER, 1, "wsdbm:gender", "wsdbm:Gender1"),
    (PRODUCT, 0, "rdf:type", "wsdbm:ProductCategory0"),
    (PRODUCT, 0, "sorg:caption", "\"guaranteed caption\""),
    (PRODUCT, 0, "sorg:text", "\"guaranteed text\""),
    (PRODUCT, 0, "sorg:contentRating", "\"Rating0\""),
    (PRODUCT, 0, "sorg:contentSize", "\"100\"^^xsd:integer"),
    (PRODUCT, 0, "sorg:description", "\"guaranteed description\""),
    (PRODUCT, 0, "sorg:keywords", "\"guaranteed keywords\""),
    (PRODUCT, 0, "sorg:language", "wsdbm:Language0"),
    (PRODUCT, 0, "sorg:publisher", "\"Publisher0\""),
    (PRODUCT, 0, "og:title", "\"guaranteed title\""),
    (PRODUCT, 0, "og:tag", "wsdbm:Topic0"),
    (PRODUCT, 0, "wsdbm:hasGenre", "wsdbm:SubGenre0"),
    (PRODUCT, 0, "foaf:homepage", "wsdbm:Website0"),
    (PRODUCT, 0, "sorg:actor", "wsdbm:User0"),
    (PRODUCT, 0, "mo:artist", "wsdbm:User0"),
    (PRODUCT, 0, "mo:conductor", "wsdbm:User1"),
    (PRODUCT, 1, "rdf:type", "wsdbm:ProductCategory2"),
    (PRODUCT, 1, "sorg:trailer", "\"Trailer1\""),
    (PRODUCT, 1, "sorg:keywords", "\"guaranteed keywords\""),
    (PRODUCT, 1, "wsdbm:hasGenre", "wsdbm:SubGenre0"),
    (REVIEW, 0, "rev:title", "\"guaranteed review\""),
    (REVIEW, 0, "rev:reviewer", "wsdbm:User0"),
    (REVIEW, 0, "rev:totalVotes", "\"7\"^^xsd:integer"),
    (OFFER, 0, "gr:includes", "wsdbm:Product0"),
    (OFFER, 0, "gr:price", "\"9.99\""),
    (OFFER, 0, "gr:serialNumber", "\"SN0\""),
    (OFFER, 0, "gr:validFrom", "\"2020-01-01\""),
    (OFFER, 0, "gr:validThrough", "\"2030-01-01\""),
    (OFFER, 0, "sorg:eligibleQuantity", "\"1\"^^xsd:integer"),
    (OFFER, 0, "sorg:eligibleRegion", "wsdbm:Country5"),
    (OFFER, 0, "sorg:eligibleRegion", "wsdbm:Country1"),
    (OFFER, 0, "sorg:priceValidUntil", "\"2030-01-01\""),
    (RETAILER, 0, "sorg:legalName", "\"Retailer Zero Ltd\""),
    (PURCHASE, 0, "wsdbm:purchaseFor", "wsdbm:Product0"),
    (PURCHASE, 0, "wsdbm:purchaseDate", "\"2020-02-02\""),
    (WEBSITE, 0, "sorg:url", "\"http://www.website0.example/\""),
    (WEBSITE, 0, "wsdbm:hits", "\"100\"^^xsd:integer"),
    (WEBSITE, 0, "sorg:language", "wsdbm:Language0"),
    (CITY, 0, "gn:parentCountry", "wsdbm:Country1"),
    (SUBGENRE, 0, "og:tag", "wsdbm:Topic0"),
];

const WORDS: [&str; 64] = [
    "alpha", "amber", "anchor", "apple", "arrow", "autumn", "badge", "bamboo", "beacon", "binary",
    "blossom", "bridge", "canyon", "carbon", "cedar", "cinder", "cobalt", "comet", "coral",
    "crystal", "delta", "desert", "ember", "falcon", "fern", "forest", "galaxy", "garden",
    "glacier", "granite", "harbor", "hollow", "horizon", "island", "jasmine", "jungle", "lagoon",
    "lantern", "lemon", "marble", "meadow", "meteor", "mirror", "nebula", "ocean", "orbit",
    "pepper", "pixel", "prairie", "quartz", "rapid", "river", "saffron", "shadow", "silver",
    "summit", "thunder", "timber", "tundra", "velvet", "violet", "willow", "winter", "zephyr",
];

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// A Zipf sampler over `0..n` with integer weights (platform-independent).
struct Zipf {
    cum: Vec<u64>,
}

impl Zipf {
    fn new(n: u64, half: bool) -> Zipf {
        let mut acc = 0u64;
        let cum = (1..=n.max(1))
            .map(|k| {
                let w = if half {
                    (1u64 << 48) / (k << 32).isqrt()
                } else {
                    (1u64 << 40) / k
                };
                acc += w.max(1);
                acc
            })
            .collect();
        Zipf { cum }
    }

    fn sample(&self, rng: &mut SplitMix64) -> u64 {
        let total = *self.cum.last().expect("non-empty");
        let u = rng.below(total);
        self.cum.partition_point(|&c| c <= u) as u64
    }
}

const TAG_CLASS: u64 = 0x7761_7463_6c73; // "watcls"
const TAG_OWN: u64 = 0x7761_746f_776e; // "watown"
const TAG_CHUNK: u64 = 0x7761_7463_686b; // "watchk"
const TAG_QUERY: u64 = 0x7761_7471_7279; // "watqry"

/// The WatDiv-like workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatDiv {
    /// The scale factor (WatDiv's `-s`).
    pub scale: u32,
    pub seed: u64,
    /// Divide the scalable classes by this (tests; `None` = 1).
    pub shrink: Option<u32>,
}

/// One line writer that never repeats a triple of the current subject.
struct Out<'a> {
    w: &'a mut dyn Write,
    line: String,
    lines: u64,
    seen: HashSet<(usize, String)>,
}

impl Out<'_> {
    fn start_subject(&mut self) {
        self.seen.clear();
    }

    /// Write `s p o` unless this subject already has it.
    fn t(&mut self, s: &str, pi: usize, p: &str, o: &str) -> io::Result<bool> {
        if !self.seen.insert((pi, o.to_string())) {
            return Ok(false);
        }
        self.line.clear();
        self.line.push_str(s);
        self.line.push(' ');
        self.line.push_str(p);
        self.line.push(' ');
        self.line.push_str(o);
        self.line.push_str(" .\n");
        self.lines += 1;
        self.w.write_all(self.line.as_bytes())?;
        Ok(true)
    }
}

/// Everything generation shares: counts, rendered predicates, Zipf tables.
struct Gen<'a> {
    w: &'a WatDiv,
    counts: [u64; 17],
    zipf: BTreeMap<(usize, bool), Zipf>,
    preds: BTreeMap<&'static str, String>,
    guaranteed: BTreeMap<(usize, u64), Vec<(&'static str, String)>>,
}

impl<'a> Gen<'a> {
    fn new(w: &'a WatDiv) -> Gen<'a> {
        let counts = std::array::from_fn(|c| w.count(c));
        let mut zipf = BTreeMap::new();
        let mut preds = BTreeMap::new();
        let mut want = |c: usize, pick: Pick| match pick {
            Pick::Uniform => {}
            Pick::Zipf1 | Pick::ZipfHalf => {
                let half = pick == Pick::ZipfHalf;
                zipf.entry((c, half))
                    .or_insert_with(|| Zipf::new(counts[c], half));
            }
        };
        for c in 0..CLASSES.len() {
            for pr in props(c) {
                preds.insert(pr.pred, expand(pr.pred));
                if let Obj::Ref(t, pick) = pr.obj {
                    want(t, pick);
                }
            }
        }
        for o in OWNS {
            preds.insert(o.pred, expand(o.pred));
            want(o.parent, o.pick);
        }
        let mut guaranteed: BTreeMap<(usize, u64), Vec<(&'static str, String)>> = BTreeMap::new();
        for (c, i, pr, o) in GUARANTEED {
            preds.insert(pr, expand(pr));
            guaranteed
                .entry((*c, *i))
                .or_default()
                .push((pr, expand(o)));
        }
        Gen {
            w,
            counts,
            zipf,
            preds,
            guaranteed,
        }
    }

    fn pick(&self, rng: &mut SplitMix64, c: usize, pick: Pick) -> u64 {
        match pick {
            Pick::Uniform => rng.below(self.counts[c]),
            Pick::Zipf1 => self.zipf[&(c, false)].sample(rng),
            Pick::ZipfHalf => self.zipf[&(c, true)].sample(rng),
        }
    }

    /// The object text of `obj` for subject `local`.
    fn object(&self, rng: &mut SplitMix64, obj: Obj, local: &str) -> String {
        match obj {
            Obj::Ref(c, pick) => entity(c, self.pick(rng, c, pick)),
            Obj::Iri(t) => expand(t),
            Obj::Name(prefix, n) => format!("\"{prefix}{}\"", rng.below(n)),
            Obj::Words(lo, hi) => {
                let k = rng.range_inclusive(lo, hi);
                let mut s = String::from("\"");
                for i in 0..k {
                    if i > 0 {
                        s.push(' ');
                    }
                    s.push_str(WORDS[rng.index(WORDS.len())]);
                }
                s.push('"');
                s
            }
            Obj::Int(lo, hi) => format!(
                "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                rng.range_inclusive(lo, hi)
            ),
            Obj::Date => format!(
                "\"{}-{:02}-{:02}\"",
                rng.range_inclusive(2000, 2025),
                rng.range_inclusive(1, 12),
                rng.range_inclusive(1, 28)
            ),
            Obj::Price => format!("\"{}.{:02}\"", rng.range_inclusive(1, 999), rng.below(100)),
            Obj::Email => format!("\"{}@example.org\"", local.to_ascii_lowercase()),
            Obj::Url => format!("\"http://www.{}.example/\"", local.to_ascii_lowercase()),
            Obj::Label => format!("\"{local}\""),
        }
    }

    /// Write entity `i` of class `c`: guaranteed triples, random properties,
    /// owned children.
    fn entity(
        &self,
        out: &mut Out<'_>,
        rng: &mut SplitMix64,
        c: usize,
        i: u64,
        children: &[(usize, &[u64])],
    ) -> io::Result<()> {
        out.start_subject();
        let local = format!("{}{i}", CLASSES[c].name);
        let s = format!("<{WSDBM}{local}>");
        let given = self.guaranteed.get(&(c, i));
        if let Some(g) = given {
            for (pr, o) in g {
                out.t(&s, pred_key(pr), &self.preds[pr], o)?;
            }
        }
        for pr in props(c) {
            let single = pr.n == ONE;
            if single && given.is_some_and(|g| g.iter().any(|(q, _)| *q == pr.pred)) {
                continue;
            }
            if rng.below(1000) >= pr.permille {
                continue;
            }
            let mut k = rng.range_inclusive(pr.n.0, pr.n.1);
            if let Obj::Ref(t, _) = pr.obj {
                // At most half the candidates (self excluded), so that a
                // skewed draw of distinct targets ends quickly.
                k = k.min((self.counts[t].saturating_sub(1) / 2).max(1));
            }
            let pk = pred_key(pr.pred);
            let mut written = 0;
            let mut draws = 0;
            while written < k && draws < 8 * k + 16 {
                draws += 1;
                let o = self.object(rng, pr.obj, &local);
                if matches!(pr.obj, Obj::Ref(t, _) if t == c) && o == s {
                    continue;
                }
                if out.t(&s, pk, &self.preds[pr.pred], &o)? {
                    written += 1;
                }
            }
        }
        for (own, kids) in children {
            let o = OWNS[*own];
            let pk = pred_key(o.pred);
            for &k in *kids {
                out.t(&s, pk, &self.preds[o.pred], &entity(o.child, k))?;
            }
        }
        Ok(())
    }

    /// The parent of every child of relation `own`.
    fn owners(&self, own: usize) -> Vec<Vec<u64>> {
        let o = OWNS[own];
        let mut rng = SplitMix64::new(stream_seed(self.w.seed, TAG_OWN, own as u64, 0));
        let mut by_parent = vec![Vec::new(); self.counts[o.parent] as usize];
        for child in 0..self.counts[o.child] {
            let parent = if child == 0 {
                0
            } else {
                self.pick(&mut rng, o.parent, o.pick)
            };
            by_parent[parent as usize].push(child);
        }
        by_parent
    }
}

/// A stable small key per predicate (for the per-subject duplicate check).
fn pred_key(p: &str) -> usize {
    crate::hash::fnv64(p.as_bytes()) as usize
}

/// The IRI of entity `i` of class `c`.
pub fn entity(c: usize, i: u64) -> String {
    format!("<{WSDBM}{}{i}>", CLASSES[c].name)
}

impl WatDiv {
    /// The number of entities of class `c`.
    pub fn count(&self, c: usize) -> u64 {
        let k = &CLASSES[c];
        if k.scalable {
            (k.base * u64::from(self.scale) / u64::from(self.shrink.unwrap_or(1).max(1))).max(1)
        } else {
            k.base
        }
    }

    /// The expected number of triples whose subject is of class `c` (the
    /// design composition: every property's probability times its mean
    /// count, plus the owned children per parent).
    pub fn expected_triples(&self, c: usize) -> f64 {
        let mut per = 0.0;
        for pr in props(c) {
            let mut mean = (pr.n.0 + pr.n.1) as f64 / 2.0;
            if let Obj::Ref(t, _) = pr.obj {
                mean = mean.min((self.count(t).saturating_sub(1) / 2).max(1) as f64);
            }
            per += pr.permille as f64 / 1000.0 * mean;
        }
        let mut total = per * self.count(c) as f64;
        for o in OWNS {
            if o.parent == c {
                total += self.count(o.child) as f64;
            }
        }
        total
    }

    fn write(&self, w: &mut dyn Write) -> io::Result<GenStats> {
        let g = Gen::new(self);
        let mut out = Out {
            w,
            line: String::with_capacity(256),
            lines: 0,
            seen: HashSet::new(),
        };
        let owners: Vec<Vec<Vec<u64>>> = (0..OWNS.len()).map(|o| g.owners(o)).collect();
        for c in 0..CLASSES.len() {
            let mut rng = SplitMix64::new(stream_seed(self.seed, TAG_CLASS, c as u64, 0));
            for i in 0..g.counts[c] {
                let kids: Vec<(usize, &[u64])> = OWNS
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| o.parent == c)
                    .map(|(k, _)| (k, owners[k][i as usize].as_slice()))
                    .collect();
                g.entity(&mut out, &mut rng, c, i, &kids)?;
            }
        }
        Ok(GenStats {
            lines: out.lines,
            unique_triples: out.lines,
            ..GenStats::default()
        })
    }

    fn write_chunk(&self, w: &mut dyn Write) -> io::Result<GenStats> {
        let g = Gen::new(self);
        let mut out = Out {
            w,
            line: String::with_capacity(256),
            lines: 0,
            seen: HashSet::new(),
        };
        let mut rng = SplitMix64::new(stream_seed(self.seed, TAG_CHUNK, 0, 0));
        let first = g.counts[USER];
        let n = (CHUNK_USERS / u64::from(self.shrink.unwrap_or(1).max(1))).max(1);
        for i in first..first + n {
            g.entity(&mut out, &mut rng, USER, i, &[])?;
        }
        Ok(GenStats {
            lines: out.lines,
            unique_triples: out.lines,
            ..GenStats::default()
        })
    }

    /// The templates, parsed.
    pub fn templates() -> Result<Vec<Template>, String> {
        TEMPLATES
            .iter()
            .map(|(name, text)| Template::parse(name, text))
            .collect()
    }

    /// Instantiate every template by an ASK walk: `ask` answers an `ASK`
    /// query against the generated data.
    pub fn instantiate(
        &self,
        ask: &mut dyn FnMut(&str) -> Result<bool, String>,
    ) -> Result<Vec<BoundQuery>, String> {
        let mut out = Vec::new();
        for (ti, t) in Self::templates()?.iter().enumerate() {
            let mut rng = SplitMix64::new(stream_seed(self.seed, TAG_QUERY, ti as u64, 0));
            let classes: Vec<usize> = t
                .mappings
                .iter()
                .map(|m| {
                    CLASSES
                        .iter()
                        .position(|c| c.name == m.class)
                        .ok_or_else(|| format!("{}: unknown class wsdbm:{}", t.name, m.class))
                })
                .collect::<Result<_, _>>()?;
            let tries = if t.mappings.is_empty() { 1 } else { MAX_TRIES };
            let mut found = None;
            for k in 0..tries {
                let bindings: Vec<(String, String)> = t
                    .mappings
                    .iter()
                    .zip(&classes)
                    .map(|(m, &c)| {
                        let i = if k + 1 == MAX_TRIES {
                            0
                        } else {
                            rng.below(self.count(c))
                        };
                        (m.var.clone(), format!("wsdbm:{}{i}", CLASSES[c].name))
                    })
                    .collect();
                if ask(&t.ask(&bindings))? {
                    found = Some((bindings, k + 1));
                    break;
                }
            }
            let (bindings, tries) = found.ok_or_else(|| {
                format!(
                    "template {}: no binding answered in {tries} tr{} (watdiv-like data of {})",
                    t.name,
                    if tries == 1 { "y" } else { "ies" },
                    self.label()
                )
            })?;
            out.push(BoundQuery {
                name: t.name.clone(),
                text: t.select(&bindings),
                bindings,
                tries,
            });
        }
        Ok(out)
    }

    /// Instantiate the templates against `data` (an N-Triples file of this
    /// workload) loaded into an in-memory database.
    pub fn instantiate_from_file(&self, data: &Path) -> Result<Vec<BoundQuery>, String> {
        let mut db = gstore::Database::build_from_files("watdiv", &[data])
            .map_err(|e| format!("loading {}: {e}", data.display()))?;
        db.set_query_cache_size(0);
        self.instantiate(&mut |q: &str| match db.query(q) {
            Ok(gstore::QueryResult::Ask(b)) => Ok(b),
            Ok(other) => Err(format!("an ASK answered {other:?}")),
            Err(e) => Err(format!("ASK failed: {e}\n{q}")),
        })
    }

    fn label(&self) -> String {
        match self.shrink {
            Some(k) if k > 1 => format!("scale {} / {k}", self.scale),
            _ => format!("scale {}", self.scale),
        }
    }
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// A `#mapping vN wsdbm:Class distribution` header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub var: String,
    pub class: String,
    pub distribution: String,
}

/// A parsed template: `#mapping` lines, `PREFIX` lines, one `SELECT … WHERE
/// {` line, the pattern lines and the closing `}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub name: String,
    pub mappings: Vec<Mapping>,
    prefixes: Vec<String>,
    select: String,
    body: Vec<String>,
}

impl Template {
    pub fn parse(name: &str, text: &str) -> Result<Template, String> {
        let err = |m: &str| format!("template {name}: {m}");
        let mut mappings = Vec::new();
        let mut saw_mapping = false;
        let mut prefixes = Vec::new();
        let mut select = None;
        let mut body = Vec::new();
        let mut closed = false;
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() {
                continue;
            }
            if closed {
                return Err(err("text after the closing brace"));
            }
            if let Some(rest) = l.strip_prefix("#mapping") {
                saw_mapping = true;
                let f: Vec<&str> = rest.split_whitespace().collect();
                match f.as_slice() {
                    ["none"] => {}
                    [var, class, dist] => {
                        let class = class
                            .strip_prefix("wsdbm:")
                            .ok_or_else(|| err("a mapping's class must be wsdbm:"))?;
                        if *dist != "uniform" {
                            return Err(err(&format!("distribution {dist} (only uniform)")));
                        }
                        mappings.push(Mapping {
                            var: var.to_string(),
                            class: class.to_string(),
                            distribution: dist.to_string(),
                        });
                    }
                    _ => return Err(err(&format!("bad mapping line {l:?}"))),
                }
            } else if l.starts_with('#') {
                continue;
            } else if l.starts_with("PREFIX") {
                prefixes.push(l.to_string());
            } else if select.is_none() {
                if !(l.starts_with("SELECT") && l.ends_with('{')) {
                    return Err(err("expected a `SELECT … WHERE {` line"));
                }
                select = Some(l.to_string());
            } else if l == "}" {
                closed = true;
            } else {
                body.push(l.to_string());
            }
        }
        if !saw_mapping {
            return Err(err("no #mapping header"));
        }
        let select = select.ok_or_else(|| err("no SELECT line"))?;
        if !closed || body.is_empty() {
            return Err(err("no pattern, or no closing brace"));
        }
        let joined = body.join("\n");
        for m in &mappings {
            if !joined.contains(&format!("%{}%", m.var)) {
                return Err(err(&format!("placeholder %{}% is not used", m.var)));
            }
        }
        let t = Template {
            name: name.to_string(),
            mappings,
            prefixes,
            select,
            body,
        };
        let probe: Vec<(String, String)> = t
            .mappings
            .iter()
            .map(|m| (m.var.clone(), "wsdbm:X".into()))
            .collect();
        if t.substitute(&probe).contains('%') {
            return Err(err("a placeholder without a #mapping line"));
        }
        Ok(t)
    }

    fn substitute(&self, bindings: &[(String, String)]) -> String {
        let mut body = self.body.join("\n  ");
        for (var, value) in bindings {
            body = body.replace(&format!("%{var}%"), value);
        }
        body
    }

    fn prologue(&self) -> String {
        let mut s = String::new();
        for p in &self.prefixes {
            let _ = writeln!(s, "{p}");
        }
        s
    }

    /// The SELECT query with `bindings` substituted.
    pub fn select(&self, bindings: &[(String, String)]) -> String {
        format!(
            "{}{}\n  {}\n}}\n",
            self.prologue(),
            self.select,
            self.substitute(bindings)
        )
    }

    /// The ASK form of the query with `bindings` substituted.
    pub fn ask(&self, bindings: &[(String, String)]) -> String {
        format!(
            "{}ASK {{\n  {}\n}}\n",
            self.prologue(),
            self.substitute(bindings)
        )
    }
}

impl Workload for WatDiv {
    fn id(&self) -> &'static str {
        "watdiv-like"
    }

    fn version(&self) -> &'static str {
        VERSION
    }

    fn file_stem(&self) -> String {
        let mut s = format!("watdiv-like1-x{}-s{}", self.scale, self.seed);
        if let Some(k) = self.shrink.filter(|&k| k > 1) {
            let _ = write!(s, "-k{k}");
        }
        s
    }

    fn db_name(&self) -> &'static str {
        "watdiv"
    }

    fn generate(&self, out: &mut dyn Write) -> io::Result<GenStats> {
        self.write(out)
    }

    fn generate_chunk(&self, out: &mut dyn Write) -> io::Result<GenStats> {
        self.write_chunk(out)
    }

    /// The templates, unbound (with their `%vN%` placeholders): the runner
    /// binds them first ([`Workload::needs_binding`]).
    fn queries(&self) -> Vec<QuerySpec> {
        TEMPLATES
            .iter()
            .map(|(name, text)| QuerySpec {
                name: (*name).to_string(),
                text: (*text).to_string(),
            })
            .collect()
    }

    fn conc_queries(&self) -> Vec<String> {
        vec!["S2".into(), "F5".into()]
    }

    fn single_insert(&self, i: usize) -> String {
        format!(
            "INSERT DATA {{ <http://www.gstore-bench.org/single/{i}> \
             <http://www.gstore-bench.org/watdiv/note> \"single{i}\" . }}"
        )
    }

    fn single_delete(&self, i: usize) -> String {
        format!(
            "DELETE DATA {{ <http://www.gstore-bench.org/single/{i}> \
             <http://www.gstore-bench.org/watdiv/note> \"single{i}\" . }}"
        )
    }

    fn needs_binding(&self) -> bool {
        true
    }

    fn bind_queries(&self, data: &Path) -> Result<Vec<BoundQuery>, String> {
        self.instantiate_from_file(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_parse_and_bind() {
        let ts = WatDiv::templates().unwrap();
        assert_eq!(ts.len(), 20);
        for t in &ts {
            let probe: Vec<(String, String)> = t
                .mappings
                .iter()
                .map(|m| (m.var.clone(), format!("wsdbm:{}0", m.class)))
                .collect();
            let q = t.select(&probe);
            assert!(!q.contains('%'), "{}: {q}", t.name);
            assert!(gstore::parser::sparql::parse(&q).is_ok(), "{}: {q}", t.name);
            assert!(gstore::parser::sparql::parse(&t.ask(&probe)).is_ok());
        }
        let constant_free: Vec<&str> = ts
            .iter()
            .filter(|t| t.mappings.is_empty())
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(constant_free, ["C1", "C2", "C3"]);
        assert!(Template::parse("x", "SELECT ?a WHERE {\n?a ?b ?c .\n}\n").is_err());
        assert!(Template::parse(
            "x",
            "#mapping v1 wsdbm:User uniform\nSELECT ?a WHERE {\n?a ?b ?c .\n}\n"
        )
        .is_err());
        assert!(Template::parse(
            "x",
            "#mapping v1 wsdbm:User zipf\nSELECT ?a WHERE {\n?a ?b %v1% .\n}\n"
        )
        .is_err());
    }

    #[test]
    fn zipf_is_skewed_and_in_range() {
        let z = Zipf::new(100, false);
        let mut rng = SplitMix64::new(1);
        let mut hits = [0u32; 100];
        for _ in 0..10_000 {
            hits[z.sample(&mut rng) as usize] += 1;
        }
        assert!(hits[0] > hits[1] && hits[1] > hits[9] && hits[9] > hits[99]);
        let h = Zipf::new(3, true);
        for _ in 0..100 {
            assert!(h.sample(&mut rng) < 3);
        }
    }

    #[test]
    fn counts_scale_and_shrink() {
        let w = WatDiv {
            scale: 10,
            seed: 0,
            shrink: None,
        };
        assert_eq!(w.count(USER), 10_000);
        assert_eq!(w.count(CITY), 240);
        let s = WatDiv {
            scale: 1,
            seed: 0,
            shrink: Some(100),
        };
        assert_eq!(s.count(RETAILER), 1);
        assert_eq!(s.count(USER), 10);
    }
}
