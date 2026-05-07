#![feature(rustc_private)]

use rust_verify::util::{VerusBuildProfile, verus_build_info};

extern crate rustc_driver;
extern crate rustc_log;
extern crate rustc_session;

#[cfg(target_family = "windows")]
fn os_setup() -> Result<(), Box<dyn std::error::Error>> {
    // Configure Windows to kill the child SMT process if the parent is killed
    let job = win32job::Job::create()?;
    let mut info = job.query_extended_limit_info()?;
    info.limit_kill_on_job_close();
    job.set_extended_limit_info(&mut info)?;
    job.assign_current_process()?;
    // dropping the job object would kill us immediately, so just let it live forever instead:
    std::mem::forget(job);
    Ok(())
}

#[cfg(target_family = "unix")]
fn os_setup() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

pub fn main() {
    let mut dep_tracker = rust_verify::cargo_verus_dep_tracker::DepTracker::init();
    let via_cargo = dep_tracker.compare_env(rust_verify::cargo_verus::VERUS_DRIVER_VIA_CARGO, "1");
    // For now, verus_builtin, vstd, etc. must be rebuilt for each via_cargo crate:
    let via_cargo_rebuild_verus_libs = via_cargo;

    let mut internal_args = std::env::args();
    let internal_program = internal_args.next().unwrap();
    let (build_test_mode, has_rustc) = if let Some(first_arg) = internal_args.next() {
        match first_arg.as_str() {
            rust_verify::trait_check::TC_DRIVER_ARG => {
                let mut internal_args: Vec<_> = internal_args.collect();
                internal_args.insert(0, internal_program);
                let mut buffer = String::new();
                use std::io::Read;
                std::io::stdin().read_to_string(&mut buffer).expect("cannot read stdin");
                rust_verify::trait_check::trait_check_rustc_driver(&internal_args[..], buffer);
                return;
            }
            arg if arg.contains("rustc") => {
                // Setting RUSTC_WRAPPER causes Cargo to pass rustc path as the first argument.
                (false, true)
            }
            "--internal-test-mode" => (true, false),
            _ => (false, false),
        }
    } else {
        (false, false)
    };

    let build_info = verus_build_info();

    let total_time_0 = std::time::Instant::now();

    let _ = os_setup();
    vir::util::set_verus_github_bug_report_url(
        ::rust_verify::consts::VERUS_GITHUB_BUG_REPORT_URL.to_owned(),
    );
    let logger_handler =
        rustc_session::EarlyDiagCtxt::new(rustc_session::config::ErrorOutputType::default());
    rustc_driver::init_logger(&logger_handler, rustc_log::LoggerConfig::from_env("RUSTVERIFY_LOG"));

    if via_cargo != has_rustc {
        let _ = logger_handler.early_err("Error: VERUS_DRIVER_VIA_CARGO must be 1 if and only if 'rustc' is the first argument to verus");
        std::process::exit(1);
    }

    let mut args = if build_test_mode || via_cargo { internal_args } else { std::env::args() };
    let program =
        if build_test_mode || via_cargo { internal_program } else { args.next().unwrap() };

    let mut vstd = None;
    let verus_root = if !(build_test_mode || via_cargo_rebuild_verus_libs) {
        let verus_root = rust_verify::driver::find_verusroot();
        if let Some(rust_verify::driver::VerusRoot { path: verusroot, .. }) = &verus_root {
            let vstd_path = verusroot.join("vstd.vir").to_str().unwrap().to_string();
            vstd = Some((format!("vstd"), vstd_path));
        }
        verus_root
    } else {
        None
    };

    let mut args: Vec<String> = args.collect();
    let is_direct_rustc_call = via_cargo
        && rust_verify::cargo_verus::extend_args_and_check_is_direct_rustc_call(
            &mut args,
            &mut dep_tracker,
        );

    if is_direct_rustc_call {
        args.insert(0, program.clone());
        rust_verify::driver::run_rustc_compiler_directly(&args);
        return;
    }

    let via_cargo = via_cargo.then(|| rust_verify::config::parse_cargo_args(&program, &mut args));

    let (our_args, rustc_args) =
        rust_verify::config::parse_args_with_imports(&program, args.into_iter(), vstd);

    if our_args.version {
        if our_args.output_json {
            println!(
                "{}",
                serde_json::ser::to_string_pretty(&build_info.to_json()).expect("invalid json")
            );
        } else {
            println!("{}", build_info);
        }
        return;
    }

    let via_cargo_compile = via_cargo
        .as_ref()
        .map(|args| rust_verify::cargo_verus::is_compile(args, &mut dep_tracker))
        .unwrap_or(false);

    if !build_test_mode {
        match build_info.profile {
            VerusBuildProfile::Debug => eprintln!(
                "warning: verus was compiled in debug mode, which will result in worse performance"
            ),
            VerusBuildProfile::Unknown => eprintln!(
                "warning: verus was compiled outside vargo, and we cannot determine whether it was built in debug mode, which will result in worse performance"
            ),
            VerusBuildProfile::Release => (),
        }
    }

    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("RUSTC_BOOTSTRAP", "1") };

    let verifier =
        rust_verify::verifier::Verifier::new(our_args, via_cargo, via_cargo_compile, dep_tracker);

    let (verifier, stats, status) = rust_verify::driver::run(
        verifier,
        rustc_args,
        verus_root,
        build_test_mode || via_cargo_rebuild_verus_libs,
    );

    let total_time_1 = std::time::Instant::now();
    let total_time = total_time_1 - total_time_0;

    let times_ms_json_data = if verifier.args.time {
        fn compute_total(
            verifier: &rust_verify::verifier::Verifier,
            f: impl Fn(&rust_verify::verifier::BucketStats) -> std::time::Duration,
        ) -> u128 {
            verifier.bucket_stats.values().map(|v| f(v)).sum::<std::time::Duration>().as_millis()
        }

        // One record per bucket carrying every stat we report.
        // Spinoff (BucketId::Fun) buckets show up as "module#function".
        struct FuncSmt<'a> {
            f: &'a vir::ast::Fun,
            time_ms: u128,
            time_micros: u128,
            rlimit: Option<u64>,
        }
        struct BucketMetrics<'a> {
            bid: &'a rust_verify::buckets::BucketId,
            name: String,
            total_ms: u128,
            air_ms: u128,
            smt_init_ms: u128,
            smt_init_micros: u128,
            smt_init_rlimit: Option<u64>,
            smt_run_ms: u128,
            smt_run_micros: u128,
            smt_run_rlimit: Option<u64>,
            functions: Vec<FuncSmt<'a>>,
        }

        let bucket_metrics: Vec<BucketMetrics> = {
            let mut v: Vec<BucketMetrics> = verifier
                .bucket_stats
                .iter()
                .map(|(bid, bs)| {
                    let air = bs.time_air.saturating_sub(bs.time_smt_init + bs.time_smt_run);
                    let mut functions: Vec<FuncSmt> = verifier
                        .func_times
                        .get(bid)
                        .map(|m| {
                            m.iter()
                                .map(|(f, t)| FuncSmt {
                                    f,
                                    time_ms: t.smt_time.as_millis(),
                                    time_micros: t.smt_time.as_micros(),
                                    rlimit: t.rlimit_count,
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    functions.sort_by_key(|fs| std::cmp::Reverse(fs.time_micros));
                    BucketMetrics {
                        bid,
                        name: bid.display_name(),
                        total_ms: bs.time_verify.as_millis(),
                        air_ms: air.as_millis(),
                        smt_init_ms: bs.time_smt_init.as_millis(),
                        smt_init_micros: bs.time_smt_init.as_micros(),
                        smt_init_rlimit: bs.rlimit_count.map(|x| x.0),
                        smt_run_ms: bs.time_smt_run.as_millis(),
                        smt_run_micros: bs.time_smt_run.as_micros(),
                        smt_run_rlimit: bs.rlimit_count.map(|x| x.1),
                        functions,
                    }
                })
                .collect();
            v.sort_by_key(|b| std::cmp::Reverse(b.total_ms));
            v
        };

        let total_verify: u128 = compute_total(&verifier, |v| v.time_verify);
        let total_air: u128 =
            compute_total(&verifier, |v| v.time_air.saturating_sub(v.time_smt_init + v.time_smt_run));
        let total_smt_init: u128 = compute_total(&verifier, |v| v.time_smt_init);
        let total_smt_run: u128 = compute_total(&verifier, |v| v.time_smt_run);

        let total_rlimit_init: Option<u64> = bucket_metrics
            .iter()
            .map(|b| b.smt_init_rlimit)
            .fold(None, |acc, x| match (acc, x) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            });
        let total_rlimit_run: Option<u64> = bucket_metrics
            .iter()
            .map(|b| b.smt_run_rlimit)
            .fold(None, |acc, x| match (acc, x) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            });

        // Rust time:
        let rust_init = stats.time_rustc;
        let trait_conflicts = stats.time_trait_conflicts;
        let compile = stats.time_compile;
        let rust = rust_init + trait_conflicts + compile;

        // total verification time
        let vir_rust_to_vir = verifier.time_vir_rust_to_vir; // included in verifier.time_vir
        let vir_vir_time = verifier.time_vir;
        let hir_time = verifier.time_hir;
        let import_time = verifier.time_import;
        let vir_time = hir_time + import_time + vir_vir_time;
        let verify_crate_time = verifier.time_verify_crate;

        // total verify time is now the time to verify the crate plus the vir time
        let verify = verifier.time_verify_crate + vir_time;

        // Unaccounted time is now total time minus all the other times
        let unaccounted = total_time - (rust + verify);

        let total_cpu_time = if verifier.num_threads > 1 {
            (total_time.as_millis()
                + total_verify
                + verifier.time_verify_crate_sequential.as_millis())
                - verifier.time_verify_crate.as_millis()
        } else {
            total_time.as_millis()
        };

        if verifier.args.output_json {
            // Unified per-bucket array. Spinoff buckets are "module#function";
            // each entry carries total/air/smt-init/smt-run + per-function smt
            // breakdown. Function-level resolve/AIR breakdown is intentionally
            // omitted: those costs are per-bucket overhead, not per-function.
            let module_times: Vec<serde_json::Value> = bucket_metrics
                .iter()
                .map(|bm| {
                    let function_breakdown: Vec<serde_json::Value> = if !verifier
                        .encountered_vir_error
                    {
                        bm.functions
                            .iter()
                            .map(|fs| {
                                serde_json::json!({
                                    "function": vir::ast_util::fun_as_friendly_rust_name(fs.f),
                                    "mode": verifier.get_function_mode(fs.f).map(|m| m.to_string()),
                                    "smt-time": fs.time_ms,
                                    "smt-time-micros": fs.time_micros,
                                    "rlimit": fs.rlimit,
                                    "success": !verifier.func_fails.contains(fs.f),
                                })
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    serde_json::json!({
                        "module": bm.name,
                        "bucket-kind": match bm.bid {
                            rust_verify::buckets::BucketId::Module(_) => "module",
                            rust_verify::buckets::BucketId::Fun(_, _) => "spinoff",
                        },
                        "total": bm.total_ms,
                        "air": bm.air_ms,
                        "smt-init": bm.smt_init_ms,
                        "smt-init-micros": bm.smt_init_micros,
                        "smt-init-rlimit": bm.smt_init_rlimit,
                        "smt-run": bm.smt_run_ms,
                        "smt-run-micros": bm.smt_run_micros,
                        "smt-run-rlimit": bm.smt_run_rlimit,
                        "function-breakdown": function_breakdown,
                    })
                })
                .collect();

            let times = serde_json::json!({
                "verus-build": {
                    "profile": build_info.profile.to_string(),
                    "version": build_info.version.to_string(),
                },
                "num-threads": verifier.num_threads,
                "total": total_time.as_millis(),
                "estimated-cpu-time": if verifier.num_threads > 1 {total_cpu_time} else {total_time.as_millis()},
                "rust": {
                    "total": rust.as_millis(),
                    "init-and-types": rust_init.as_millis(),
                    "trait-conflicts": trait_conflicts.as_millis(),
                    "compile": compile.as_millis(),
                },
                "verification": {
                    "total": verify.as_millis(),
                    "vir" : {
                        "total": vir_time.as_millis(),
                        "hir": hir_time.as_millis(),
                        "import": import_time.as_millis(),
                        "rust-to-vir": vir_rust_to_vir.as_millis()
                    }
                },
                "verify-totals": {
                    "total-verify": total_verify,
                    "air": total_air,
                    "smt-init": total_smt_init,
                    "smt-init-rlimit": total_rlimit_init,
                    "smt-run": total_smt_run,
                    "smt-run-rlimit": total_rlimit_run,
                },
                "module-times": module_times,
            });

            Some(times)
        } else {
            println!("(use --output-json for machine-readable output)");
            println!("verus-build-info\n{}", build_info);
            print!("total-time:      {:>10} ms", total_time.as_millis());
            if verifier.num_threads > 1 {
                println!("    (estimated total cpu time {} ms)", total_cpu_time);
            } else {
                println!();
            }
            println!("    rust-time:          {:>10} ms", rust.as_millis());
            println!("        init-and-types:     {:>10} ms", rust_init.as_millis());
            println!("        trait-conflicts:    {:>10} ms", trait_conflicts.as_millis());
            println!("        compile-time:       {:>10} ms", compile.as_millis());

            println!("    verification-time:  {:>10} ms", verify.as_millis());
            println!("        vir-time:           {:>10} ms", vir_time.as_millis());
            println!("            hir-time:           {:>10} ms", hir_time.as_millis());
            println!("            import-time:        {:>10} ms", import_time.as_millis());
            println!("            rust-to-vir:        {:>10} ms", vir_rust_to_vir.as_millis());
            println!("        verify-crate-time:  {:>10} ms", verify_crate_time.as_millis());
            println!("    unaccounted-time:   {:>10} ms", unaccounted.as_millis());

            println!("\nverify-crate-time-breakdown");
            println!(
                "    total verify-time:     {:>10} ms   ({} threads)",
                total_verify, verifier.num_threads
            );
            println!(
                "        total air-time:        {:>10} ms   ({} threads)",
                total_air, verifier.num_threads
            );
            if !verifier.encountered_vir_error {
                println!(
                    "        total smt-time:        {:>10} ms   ({} threads)",
                    (total_smt_init + total_smt_run),
                    verifier.num_threads
                );
                println!(
                    "            total smt-init:        {:>10} ms{} ({} threads)",
                    total_smt_init,
                    total_rlimit_init
                        .map(|rc| format!(", {:>8} rlimit", rc))
                        .unwrap_or(format!("")),
                    verifier.num_threads
                );
                println!(
                    "            total smt-run:         {:>10} ms{} ({} threads)",
                    total_smt_run,
                    total_rlimit_run
                        .map(|rc| format!(", {:>8} rlimit", rc))
                        .unwrap_or(format!("")),
                    verifier.num_threads,
                );
            }

            if verifier.args.time_expanded && !bucket_metrics.is_empty() {
                println!("\n    per-module breakdown (top 3 by total verify-time):");
                for (i, bm) in bucket_metrics.iter().take(3).enumerate() {
                    println!(
                        "      {}. {}{}",
                        i + 1,
                        bm.name,
                        match bm.bid {
                            rust_verify::buckets::BucketId::Fun(_, _) => "  [spinoff]",
                            _ => "",
                        }
                    );
                    println!(
                        "           total={:>7} ms   air={:>5} ms   smt-init={:>5} ms{}   smt-run={:>7} ms{}",
                        bm.total_ms,
                        bm.air_ms,
                        bm.smt_init_ms,
                        bm.smt_init_rlimit
                            .map(|rc| format!(" (rlimit {})", rc))
                            .unwrap_or_default(),
                        bm.smt_run_ms,
                        bm.smt_run_rlimit
                            .map(|rc| format!(" (rlimit {})", rc))
                            .unwrap_or_default(),
                    );
                    if !verifier.encountered_vir_error && !bm.functions.is_empty() {
                        for (j, fs) in bm.functions.iter().take(3).enumerate() {
                            println!(
                                "                {}. {:<60} smt={:>7} ms{}",
                                j + 1,
                                vir::ast_util::fun_as_friendly_rust_name(fs.f),
                                fs.time_ms,
                                fs.rlimit
                                    .map(|rc| format!(" (rlimit {})", rc))
                                    .unwrap_or_default(),
                            );
                        }
                    }
                }
            }

            None
        }
    } else {
        None
    };

    if verifier.args.output_json {
        // Render function verification details as JSON.
        let mut func_details: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for (func, details) in &verifier.func_details {
            let name = vir::ast_util::fun_as_friendly_rust_name(&func);
            func_details.insert(name, details.to_json());
        }

        let mut res = serde_json::json!({
            "encountered-error": status.is_err(),
            "encountered-vir-error": verifier.encountered_vir_error,
        });
        if rust_verify::driver::is_verifying_entire_crate(&verifier) {
            res["success"] = serde_json::json!(
                !status.is_err() && !verifier.encountered_vir_error && verifier.count_errors == 0
            );
        }
        if !verifier.encountered_vir_error {
            res.as_object_mut().unwrap().append(
                serde_json::json!({
                    "verified": verifier.count_verified,
                    "errors": verifier.count_errors,
                    "is-verifying-entire-crate": rust_verify::driver::is_verifying_entire_crate(&verifier),
                })
                .as_object_mut()
                .unwrap(),
            );
        }
        let mut out: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        out.insert("func-details".to_string(), serde_json::Value::Object(func_details));
        out.insert("verification-results".to_string(), res);
        if let Some(times_ms) = times_ms_json_data {
            out.insert("times-ms".to_string(), times_ms);
        }
        out.append(&mut build_info.to_json().as_object_mut().unwrap());
        println!("{}", serde_json::ser::to_string_pretty(&out).expect("invalid json"));
    }

    match status {
        Ok(()) => (),
        Err(_) => {
            std::process::exit(1);
        }
    }
}
