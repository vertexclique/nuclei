//! `op_count <row> <iterations>`: runs the iterations of one row (see `rows.rs`) and prints
//! `row=<row> backend=<backend> ops=<operations>`. The gate in `ci/op_counts.sh` runs it under
//! cachegrind at two iteration counts, so the setup cost cancels out.

#[path = "rows.rs"]
mod rows;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(row), Some(iterations)) = (args.next(), args.next().and_then(|n| n.parse().ok()))
    else {
        eprintln!("usage: op_count <{}> <iterations>", rows::ROWS.join("|"));
        std::process::exit(2);
    };
    let rt = rows::runtime(&row);
    let backend = rows::backend(&rt);
    let Some(ops) = rows::run(&rt, &row, iterations) else {
        eprintln!("no row {row} here; the rows are {}", rows::ROWS.join(", "));
        std::process::exit(2);
    };
    println!("row={row} backend={backend:?} ops={ops}");
}
