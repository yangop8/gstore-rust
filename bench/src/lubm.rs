//! The LUBM-like workload: a deterministic, UBA-style university data
//! generator and the 14 LUBM queries of `testdata/lubm/`.
//!
//! A port of the design-stage `gen_lubm.py` prototype (Phase 6 planning; that
//! script is not part of the repository — this module replaces it). The data
//! follows the univ-bench ontology and the cardinalities of the Lehigh UBA
//! generator:
//!
//! * 15–25 departments per university; per department 7–10 full professors,
//!   10–14 associate and 8–11 assistant professors, 5–7 lecturers;
//! * 8–14 undergraduates and 3–4 graduate students per faculty member;
//! * 1–2 courses and 1–2 graduate courses per faculty member, assigned in
//!   faculty order; undergraduates take 2–4 courses, graduates 1–3 graduate
//!   courses;
//! * 10–20 research groups; publications: full professors 15–20, associate
//!   10–18, assistant 5–10, lecturers 0–5; every graduate co-authors 0–5
//!   faculty publications;
//! * one in five undergraduates has an advisor, every graduate has one; a
//!   graduate is a research assistant with probability 1/4–1/3, else a
//!   teaching assistant with probability 1/5–1/4; one full professor heads
//!   each department;
//! * degrees come from a space of 1000 universities, each reference followed
//!   by a `rdf:type ub:University` line as UBA writes it — so the file holds
//!   **duplicate lines** (the store keeps one copy; [`GenStats::unique_triples`]
//!   counts what it keeps).
//!
//! Guarantees, so that every query is non-empty at every scale (including a
//! single department):
//! * Q1/Q10: `GraduateCourse0` of Department0.University0 has a graduate taker;
//! * Q2: in every university, GraduateStudent0 of Department0 took its
//!   undergraduate degree at its own university;
//! * Q7: AssociateProfessor0's first course in Department0.University0 has an
//!   undergraduate taker;
//! * Q9: in every department UndergraduateStudent0 is advised by
//!   FullProfessor0 and takes Course0, which FullProfessor0 teaches.
//!
//! Randomness: one [`SplitMix64`] stream per university (its department
//! count) and one per department (everything inside it), each seeded from
//! (seed, university, department) by [`stream_seed`]. The output is a pure
//! function of ([`Lubm::univ`], [`Lubm::seed`], [`Lubm::departments`]) and of
//! [`VERSION`]; `bench/tests/golden.rs` pins the hash of univ 1, seed 0.
//!
//! The update chunk ([`Workload::generate_chunk`]) is Department0 of
//! `University{univ}` — a university the data set does not generate — without
//! its `rdf:type ub:University` lines: every chunk triple has a subject in the
//! chunk department's own IRI space, so the chunk is disjoint from the data.

use std::fmt::Write as _;
use std::io::{self, Write};

use crate::rng::{stream_seed, SplitMix64};
use crate::workload::{GenStats, QuerySpec, Workload};

/// The generator version; part of every baseline's exact gates. Bump it on
/// any change to the output.
pub const VERSION: &str = "lubm-like/1";

/// UBA picks degree-granting universities from `0..UNIV_SPACE`.
pub const UNIV_SPACE: u64 = 1000;

const RDF_TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
const UB: &str = "http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#";

/// The 14 queries, verbatim from `testdata/lubm/`.
pub const QUERIES: [(&str, &str); 14] = [
    ("q1", include_str!("../../testdata/lubm/lubm_q1.rq")),
    ("q2", include_str!("../../testdata/lubm/lubm_q2.rq")),
    ("q3", include_str!("../../testdata/lubm/lubm_q3.rq")),
    ("q4", include_str!("../../testdata/lubm/lubm_q4.rq")),
    ("q5", include_str!("../../testdata/lubm/lubm_q5.rq")),
    ("q6", include_str!("../../testdata/lubm/lubm_q6.rq")),
    ("q7", include_str!("../../testdata/lubm/lubm_q7.rq")),
    ("q8", include_str!("../../testdata/lubm/lubm_q8.rq")),
    ("q9", include_str!("../../testdata/lubm/lubm_q9.rq")),
    ("q10", include_str!("../../testdata/lubm/lubm_q10.rq")),
    ("q11", include_str!("../../testdata/lubm/lubm_q11.rq")),
    ("q12", include_str!("../../testdata/lubm/lubm_q12.rq")),
    ("q13", include_str!("../../testdata/lubm/lubm_q13.rq")),
    ("q14", include_str!("../../testdata/lubm/lubm_q14.rq")),
];

/// FNV-1a 64 of each query text in [`QUERIES`], pinned: a changed query file
/// changes what every baseline measured (`bench/tests/golden.rs` checks
/// these, and the gates compare them with the baseline's).
pub const QUERY_TEXT_FNV64: [(&str, u64); 14] = [
    ("q1", 0x0689_93b9_a3b1_6c66),
    ("q2", 0xdaf8_95f0_26b9_6e4e),
    ("q3", 0x98f8_ea2a_a826_caef),
    ("q4", 0xd0f4_e7f0_d215_b4e3),
    ("q5", 0xb393_27f8_40b0_5714),
    ("q6", 0x890e_12dc_05e9_ebba),
    ("q7", 0xb6eb_a16b_74db_35d4),
    ("q8", 0x8ac4_bfc6_31ba_d540),
    ("q9", 0x81f1_cea0_bc75_9b0e),
    ("q10", 0x5237_0fd0_76e8_b359),
    ("q11", 0x63b9_9039_e02f_501b),
    ("q12", 0xd679_7b3f_b47b_9194),
    ("q13", 0xcf6d_24ce_ec0a_9497),
    ("q14", 0xb6fd_1dbf_d561_1c19),
];

/// Stream tags of [`stream_seed`]. The department streams use "dept". The
/// university streams (which only draw the department count) use "univ" +
/// 284·2³², the first such tag (k = 0, 1, …) whose department counts
/// reproduce the prototype's scale points at seed 0: University0 has 21
/// departments as in the prototype data (so univ 1 has its ~143K triples),
/// univ 8 has 156 and univ 38 has 779 departments in total.
const TAG_UNIV: u64 = 0x011c_756e_6976;
const TAG_DEPT: u64 = 0x6465_7074; // "dept"

/// The LUBM-like workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lubm {
    /// Number of universities (UBA's `-univ`).
    pub univ: u32,
    /// Generator seed (UBA's `-seed`).
    pub seed: u64,
    /// At most this many departments per university (`None` = 15–25).
    pub departments: Option<u32>,
}

fn univ_iri(u: u64) -> String {
    format!("<http://www.University{u}.edu>")
}

fn lit(s: &str) -> String {
    format!("\"{s}\"")
}

/// The univ-bench IRI of `local`.
fn ub(local: &str) -> String {
    format!("<{UB}{local}>")
}

/// Pre-rendered predicate and class IRIs.
struct Vocab {
    name: String,
    sub_organization_of: String,
    teacher_of: String,
    degree: [String; 3],
    works_for: String,
    email: String,
    telephone: String,
    research_interest: String,
    head_of: String,
    member_of: String,
    takes_course: String,
    advisor: String,
    teaching_assistant_of: String,
    publication_author: String,
    c_university: String,
    c_department: String,
    c_kind: [String; 4],
    c_undergrad: String,
    c_grad: String,
    c_course: String,
    c_grad_course: String,
    c_research_group: String,
    c_publication: String,
    c_research_assistant: String,
    c_teaching_assistant: String,
}

impl Vocab {
    fn new() -> Vocab {
        Vocab {
            name: ub("name"),
            sub_organization_of: ub("subOrganizationOf"),
            teacher_of: ub("teacherOf"),
            degree: [
                ub("undergraduateDegreeFrom"),
                ub("mastersDegreeFrom"),
                ub("doctoralDegreeFrom"),
            ],
            works_for: ub("worksFor"),
            email: ub("emailAddress"),
            telephone: ub("telephone"),
            research_interest: ub("researchInterest"),
            head_of: ub("headOf"),
            member_of: ub("memberOf"),
            takes_course: ub("takesCourse"),
            advisor: ub("advisor"),
            teaching_assistant_of: ub("teachingAssistantOf"),
            publication_author: ub("publicationAuthor"),
            c_university: ub("University"),
            c_department: ub("Department"),
            c_kind: KINDS.map(|k| ub(k.class)),
            c_undergrad: ub("UndergraduateStudent"),
            c_grad: ub("GraduateStudent"),
            c_course: ub("Course"),
            c_grad_course: ub("GraduateCourse"),
            c_research_group: ub("ResearchGroup"),
            c_publication: ub("Publication"),
            c_research_assistant: ub("ResearchAssistant"),
            c_teaching_assistant: ub("TeachingAssistant"),
        }
    }
}

/// A faculty kind: its class, head-count range and publication range.
struct Kind {
    class: &'static str,
    count: (u64, u64),
    publications: (u64, u64),
}

const KINDS: [Kind; 4] = [
    Kind {
        class: "FullProfessor",
        count: (7, 10),
        publications: (15, 20),
    },
    Kind {
        class: "AssociateProfessor",
        count: (10, 14),
        publications: (10, 18),
    },
    Kind {
        class: "AssistantProfessor",
        count: (8, 11),
        publications: (5, 10),
    },
    Kind {
        class: "Lecturer",
        count: (5, 7),
        publications: (0, 5),
    },
];
const LECTURER: usize = 3;

/// The N-Triples writer with UBA's duplicate bookkeeping.
struct Out<'a> {
    w: &'a mut dyn Write,
    line: String,
    lines: u64,
    dups: u64,
    /// Universities whose `rdf:type ub:University` line was written.
    typed: Vec<bool>,
    /// Write `rdf:type ub:University` lines (off for the update chunk).
    univ_types: bool,
}

impl<'a> Out<'a> {
    fn new(w: &'a mut dyn Write, univ_types: bool) -> Out<'a> {
        Out {
            w,
            line: String::with_capacity(256),
            lines: 0,
            dups: 0,
            typed: Vec::new(),
            univ_types,
        }
    }

    fn t(&mut self, s: &str, p: &str, o: &str) -> io::Result<()> {
        self.line.clear();
        self.line.push_str(s);
        self.line.push(' ');
        self.line.push_str(p);
        self.line.push(' ');
        self.line.push_str(o);
        self.line.push_str(" .\n");
        self.lines += 1;
        self.w.write_all(self.line.as_bytes())
    }

    fn univ_type(&mut self, u: u64, v: &Vocab) -> io::Result<()> {
        if !self.univ_types {
            return Ok(());
        }
        let i = u as usize;
        if i >= self.typed.len() {
            self.typed.resize(i + 1, false);
        }
        if self.typed[i] {
            self.dups += 1;
        } else {
            self.typed[i] = true;
        }
        self.t(&univ_iri(u), RDF_TYPE, &v.c_university)
    }

    fn stats(&self) -> GenStats {
        GenStats {
            lines: self.lines,
            unique_triples: self.lines - self.dups,
            ..GenStats::default()
        }
    }
}

/// One faculty member.
struct Faculty {
    kind: usize,
    idx: u64,
    local: String,
}

impl Lubm {
    fn univ_rng(&self, u: u64) -> SplitMix64 {
        SplitMix64::new(stream_seed(self.seed, TAG_UNIV, u, 0))
    }

    fn dept_rng(&self, u: u64, d: u64) -> SplitMix64 {
        SplitMix64::new(stream_seed(self.seed, TAG_DEPT, u, d))
    }

    /// The number of departments of university `u`.
    pub fn departments_of(&self, u: u64) -> u64 {
        let n = self.univ_rng(u).range_inclusive(15, 25);
        match self.departments {
            Some(cap) => n.min(u64::from(cap)),
            None => n,
        }
    }

    fn gen_department(&self, out: &mut Out<'_>, v: &Vocab, u: u64, d: u64) -> io::Result<()> {
        let mut rng = self.dept_rng(u, d);
        let uiri = univ_iri(u);
        let dname = format!("Department{d}");
        let dbase = format!("http://www.{dname}.University{u}.edu");
        let diri = format!("<{dbase}>");
        let e = |local: &str| format!("<{dbase}/{local}>");
        let email = |local: &str| lit(&format!("{local}@{dname}.University{u}.edu"));
        let phone = lit("xxx-xxx-xxxx");

        out.t(&diri, RDF_TYPE, &v.c_department)?;
        out.t(&diri, &v.name, &lit(&dname))?;
        out.t(&diri, &v.sub_organization_of, &uiri)?;
        out.univ_type(u, v)?;

        let counts: Vec<u64> = KINDS
            .iter()
            .map(|k| rng.range_inclusive(k.count.0, k.count.1))
            .collect();
        let mut faculty: Vec<Faculty> = Vec::new();
        let mut professors: Vec<usize> = Vec::new(); // indices into faculty
        for (kind, &n) in counts.iter().enumerate() {
            for i in 0..n {
                if kind != LECTURER {
                    professors.push(faculty.len());
                }
                faculty.push(Faculty {
                    kind,
                    idx: i,
                    local: format!("{}{i}", KINDS[kind].class),
                });
            }
        }
        let nfac = faculty.len() as u64;

        // Courses: assigned sequentially in faculty order, like UBA.
        let mut course_of: Vec<(u64, u64)> = Vec::with_capacity(faculty.len()); // [start, end)
        let mut gcourse_of: Vec<(u64, u64)> = Vec::with_capacity(faculty.len());
        let (mut ncourse, mut ngcourse) = (0u64, 0u64);
        for _ in &faculty {
            let k = rng.range_inclusive(1, 2);
            course_of.push((ncourse, ncourse + k));
            ncourse += k;
            let k = rng.range_inclusive(1, 2);
            gcourse_of.push((ngcourse, ngcourse + k));
            ngcourse += k;
        }

        let head = rng.below(counts[0]);
        // (author index into faculty, publication number)
        let mut publications: Vec<(usize, u64)> = Vec::new();
        let mut buf = String::new();
        for (fi, f) in faculty.iter().enumerate() {
            let s = e(&f.local);
            out.t(&s, RDF_TYPE, &v.c_kind[f.kind])?;
            out.t(&s, &v.name, &lit(&f.local))?;
            for c in course_of[fi].0..course_of[fi].1 {
                out.t(&s, &v.teacher_of, &e(&format!("Course{c}")))?;
            }
            for c in gcourse_of[fi].0..gcourse_of[fi].1 {
                out.t(&s, &v.teacher_of, &e(&format!("GraduateCourse{c}")))?;
            }
            for pred in &v.degree {
                let du = rng.below(UNIV_SPACE);
                out.t(&s, pred, &univ_iri(du))?;
                out.univ_type(du, v)?;
            }
            out.t(&s, &v.works_for, &diri)?;
            out.t(&s, &v.email, &email(&f.local))?;
            out.t(&s, &v.telephone, &phone)?;
            if f.kind != LECTURER {
                buf.clear();
                let _ = write!(buf, "Research{}", rng.below(30));
                out.t(&s, &v.research_interest, &lit(&buf))?;
            }
            if f.kind == 0 && f.idx == head {
                out.t(&s, &v.head_of, &diri)?;
            }
            let (lo, hi) = KINDS[f.kind].publications;
            for k in 0..rng.range_inclusive(lo, hi) {
                publications.push((fi, k));
            }
        }

        for c in 0..ncourse {
            let local = format!("Course{c}");
            out.t(&e(&local), RDF_TYPE, &v.c_course)?;
            out.t(&e(&local), &v.name, &lit(&local))?;
        }
        for c in 0..ngcourse {
            let local = format!("GraduateCourse{c}");
            out.t(&e(&local), RDF_TYPE, &v.c_grad_course)?;
            out.t(&e(&local), &v.name, &lit(&local))?;
        }

        // Undergraduates.
        let nug = nfac * rng.range_inclusive(8, 14);
        let mut ug_taken = vec![false; ncourse as usize];
        for i in 0..nug {
            let local = format!("UndergraduateStudent{i}");
            let s = e(&local);
            out.t(&s, RDF_TYPE, &v.c_undergrad)?;
            out.t(&s, &v.name, &lit(&local))?;
            out.t(&s, &v.member_of, &diri)?;
            out.t(&s, &v.email, &email(&local))?;
            out.t(&s, &v.telephone, &phone)?;
            let want = rng.range_inclusive(2, 4) as usize;
            let mut courses = rng.sample(ncourse as usize, want.min(ncourse as usize));
            let mut advisor = if rng.below(5) == 0 {
                Some(professors[rng.index(professors.len())])
            } else {
                None
            };
            if i == 0 {
                // Q9: advised by FullProfessor0 (faculty[0]) and taking Course0.
                advisor = Some(0);
                if !courses.contains(&0) {
                    courses[0] = 0;
                }
            }
            for &c in &courses {
                out.t(&s, &v.takes_course, &e(&format!("Course{c}")))?;
                ug_taken[c] = true;
            }
            if let Some(a) = advisor {
                out.t(&s, &v.advisor, &e(&faculty[a].local))?;
            }
        }

        // Q7: AssociateProfessor0's first course has an undergraduate taker.
        if u == 0 && d == 0 {
            let ap0 = counts[0] as usize; // AssociateProfessor0 follows the full professors
            let c = course_of[ap0].0;
            if !ug_taken[c as usize] {
                out.t(
                    &e("UndergraduateStudent1"),
                    &v.takes_course,
                    &e(&format!("Course{c}")),
                )?;
            }
        }

        // Graduate students.
        let ngrad = nfac * rng.range_inclusive(3, 4);
        let ta_ratio = rng.uniform(1.0 / 5.0, 1.0 / 4.0);
        let ra_ratio = rng.uniform(1.0 / 4.0, 1.0 / 3.0);
        let mut gc0_taken = false;
        let mut coauthors: Vec<Vec<u64>> = vec![Vec::new(); publications.len()];
        for i in 0..ngrad {
            let local = format!("GraduateStudent{i}");
            let s = e(&local);
            out.t(&s, RDF_TYPE, &v.c_grad)?;
            out.t(&s, &v.name, &lit(&local))?;
            out.t(&s, &v.member_of, &diri)?;
            out.t(&s, &v.email, &email(&local))?;
            out.t(&s, &v.telephone, &phone)?;
            let want = rng.range_inclusive(1, 3) as usize;
            for c in rng.sample(ngcourse as usize, want.min(ngcourse as usize)) {
                out.t(&s, &v.takes_course, &e(&format!("GraduateCourse{c}")))?;
                gc0_taken |= c == 0;
            }
            let mut du = rng.below(UNIV_SPACE);
            if i == 0 && d == 0 {
                du = u; // Q2
            }
            out.t(&s, &v.degree[0], &univ_iri(du))?;
            out.univ_type(du, v)?;
            let a = professors[rng.index(professors.len())];
            out.t(&s, &v.advisor, &e(&faculty[a].local))?;
            let r = rng.next_f64();
            if r < ra_ratio {
                out.t(&s, RDF_TYPE, &v.c_research_assistant)?;
            } else if r < ra_ratio + ta_ratio {
                out.t(&s, RDF_TYPE, &v.c_teaching_assistant)?;
                let c = rng.below(ncourse);
                out.t(&s, &v.teaching_assistant_of, &e(&format!("Course{c}")))?;
            }
            if !publications.is_empty() {
                let want = rng.range_inclusive(0, 5) as usize;
                for k in rng.sample(publications.len(), want.min(publications.len())) {
                    coauthors[k].push(i);
                }
            }
        }
        if u == 0 && d == 0 && !gc0_taken {
            // Q1/Q10
            out.t(&e("GraduateStudent0"), &v.takes_course, &e("GraduateCourse0"))?;
        }

        for (k, &(fi, p)) in publications.iter().enumerate() {
            let author = &faculty[fi].local;
            let s = e(&format!("{author}/Publication{p}"));
            out.t(&s, RDF_TYPE, &v.c_publication)?;
            out.t(&s, &v.name, &lit(&format!("Publication{p}")))?;
            out.t(&s, &v.publication_author, &e(author))?;
            for g in &coauthors[k] {
                out.t(&s, &v.publication_author, &e(&format!("GraduateStudent{g}")))?;
            }
        }

        for g in 0..rng.range_inclusive(10, 20) {
            let s = e(&format!("ResearchGroup{g}"));
            out.t(&s, RDF_TYPE, &v.c_research_group)?;
            out.t(&s, &v.sub_organization_of, &diri)?;
        }
        Ok(())
    }
}

impl Workload for Lubm {
    fn id(&self) -> &'static str {
        "lubm-like"
    }

    fn version(&self) -> &'static str {
        VERSION
    }

    fn file_stem(&self) -> String {
        let mut s = format!("lubm-like1-u{}-s{}", self.univ, self.seed);
        if let Some(d) = self.departments {
            let _ = write!(s, "-d{d}");
        }
        s
    }

    fn db_name(&self) -> &'static str {
        "lubm"
    }

    fn generate(&self, w: &mut dyn Write) -> io::Result<GenStats> {
        let v = Vocab::new();
        let mut out = Out::new(w, true);
        for u in 0..u64::from(self.univ) {
            out.univ_type(u, &v)?;
            out.t(&univ_iri(u), &v.name, &lit(&format!("University{u}")))?;
            for d in 0..self.departments_of(u) {
                self.gen_department(&mut out, &v, u, d)?;
            }
        }
        Ok(out.stats())
    }

    fn generate_chunk(&self, w: &mut dyn Write) -> io::Result<GenStats> {
        let v = Vocab::new();
        let mut out = Out::new(w, false);
        self.gen_department(&mut out, &v, u64::from(self.univ), 0)?;
        Ok(out.stats())
    }

    fn queries(&self) -> Vec<QuerySpec> {
        QUERIES
            .iter()
            .map(|(name, text)| QuerySpec {
                name: (*name).to_string(),
                text: (*text).to_string(),
            })
            .collect()
    }

    fn conc_queries(&self) -> Vec<String> {
        vec!["q1".into(), "q4".into()]
    }

    fn single_insert(&self, i: usize) -> String {
        format!(
            "INSERT DATA {{ <http://www.gstore-bench.org/single/{i}> <{UB}name> \"single{i}\" . }}"
        )
    }

    fn single_delete(&self, i: usize) -> String {
        format!(
            "DELETE DATA {{ <http://www.gstore-bench.org/single/{i}> <{UB}name> \"single{i}\" . }}"
        )
    }
}
