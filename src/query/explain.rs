//! Textuelle Baum-Darstellung eines Physical Plans (`explain()`).

use crate::codec::Value;
use crate::index::Bound;

use super::logical::SortDir;
use super::physical::PhysicalPlan;

/// Formatiert den Plan als eingerückten Baum.
pub fn format(plan: &PhysicalPlan) -> String {
    let mut out = String::new();
    write_node(plan, 0, &mut out);
    out
}

fn write_node(plan: &PhysicalPlan, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    out.push_str(&pad);
    match plan {
        PhysicalPlan::IndexScan {
            collection,
            field,
            lower,
            upper,
        } => {
            out.push_str(&format!(
                "IndexScan {{ collection: {collection}, field: {field}, range: {} }}",
                format_range(lower, upper)
            ));
        }
        PhysicalPlan::IndexOrderScan {
            collection,
            field,
            lower,
            upper,
            dir,
        } => {
            let dir = match dir {
                SortDir::Asc => "Asc",
                SortDir::Desc => "Desc",
            };
            out.push_str(&format!(
                "IndexOrderScan {{ collection: {collection}, field: {field}, range: {}, dir: {dir} }}",
                format_range(lower, upper)
            ));
        }
        PhysicalPlan::FullScan { collection } => {
            out.push_str(&format!("FullScan {{ collection: {collection} }}"));
        }
        PhysicalPlan::UnionIds { branches } => {
            out.push_str(&format!("UnionIds {{ branches: {} }}", branches.len()));
            for b in branches {
                out.push('\n');
                write_node(b, depth + 1, out);
            }
        }
        PhysicalPlan::Fetch { collection, .. } => {
            out.push_str(&format!("Fetch {{ collection: {collection} }}"));
            out.push('\n');
            if let Some(input) = plan.input() {
                write_node(input, depth + 1, out);
            }
        }
        PhysicalPlan::Filter { pred, .. } => {
            out.push_str(&format!("Filter {{ predicate: {} }}", format_pred(pred)));
            out.push('\n');
            if let Some(input) = plan.input() {
                write_node(input, depth + 1, out);
            }
        }
        PhysicalPlan::Sort { field, dir, .. } => {
            let dir = match dir {
                SortDir::Asc => "Asc",
                SortDir::Desc => "Desc",
            };
            out.push_str(&format!("Sort {{ field: {field}, dir: {dir} }}"));
            out.push('\n');
            if let Some(input) = plan.input() {
                write_node(input, depth + 1, out);
            }
        }
        PhysicalPlan::Limit { n, .. } => {
            out.push_str(&format!("Limit {{ n: {n} }}"));
            out.push('\n');
            if let Some(input) = plan.input() {
                write_node(input, depth + 1, out);
            }
        }
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("{b}"),
        Value::Int(i) => format!("{i}"),
        Value::Float(f) => format!("{f}"),
        Value::String(s) => format!("{s:?}"),
        Value::Bytes(b) => format!("{:02x?}", b),
    }
}

fn format_bound(b: &Bound) -> String {
    match b {
        Bound::Unbounded => "∅".to_string(),
        Bound::Inclusive(v) => format!("[{},]", format_value(v)),
        Bound::Exclusive(v) => format!("({},)", format_value(v)),
    }
}

fn format_range(lower: &Bound, upper: &Bound) -> String {
    format!("{}..{}", format_bound(lower), format_bound(upper))
}

/// Formatiert ein Prädikat für Explain (nur Anzeige).
fn format_pred(p: &super::ast::Predicate) -> String {
    use super::ast::{Cmp, Predicate};
    match p {
        Predicate::And(a, b) => format!("({} AND {})", format_pred(a), format_pred(b)),
        Predicate::Or(a, b) => format!("({} OR {})", format_pred(a), format_pred(b)),
        Predicate::Not(a) => format!("NOT({})", format_pred(a)),
        Predicate::Field(f, Cmp::Ne, v) => format!("{f} != {}", format_value(v)),
        Predicate::Field(f, Cmp::Eq, v) => format!("{f} = {}", format_value(v)),
        Predicate::Field(f, Cmp::Lt, v) => format!("{f} < {}", format_value(v)),
        Predicate::Field(f, Cmp::Lte, v) => format!("{f} <= {}", format_value(v)),
        Predicate::Field(f, Cmp::Gt, v) => format!("{f} > {}", format_value(v)),
        Predicate::Field(f, Cmp::Gte, v) => format!("{f} >= {}", format_value(v)),
    }
}
