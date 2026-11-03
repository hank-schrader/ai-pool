//! Profile planning. Existing checks stay here.
mod selection;
#[cfg(test)]
use pool_protocol::Accelerator;
pub use selection::*;
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;
    use pool_protocol::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(include_str!("../../../tests/fixtures/all-models.json")).unwrap()
    }

    #[test]
    fn both_models_on_a_16_gb_card() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        let choices = plan(&models, Accelerator::Cuda, 14888 - 256, 20000, &HashMap::new()).unwrap();
        let picked: Vec<_> = choices.iter().map(|choice| choice.profile.id.as_str()).collect();
        assert_eq!(picked, ["cuda-8192", "cuda-16384"]);
    }

    #[test]
    fn reports_conflicts_and_honors_overrides() {
        let catalog = catalog();
        let models = [catalog.model("clef").unwrap(), catalog.model("qwen2.5-1.5b-instruct").unwrap()];
        assert!(plan(&models, Accelerator::Cuda, 1000, 0, &HashMap::new()).unwrap_err().contains("do not fit"));
        let overrides = parse_overrides(&["clef=cuda-4096".into()], &["qwen2.5-1.5b-instruct=8192".into()]).unwrap();
        let choices = plan(&models, Accelerator::Cuda, 14632, 20000, &overrides).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-4096");
        assert_eq!(choices[1].profile.id, "cuda-8192");
        let bad = parse_overrides(&[], &["clef=5000".into()]).unwrap();
        assert!(plan(&models, Accelerator::Cuda, 14632, 20000, &bad).is_err());
    }

    #[test]
    fn offloads_only_when_the_model_does_not_fit_on_the_gpu() {
        let catalog = catalog();
        let clef = [catalog.model("clef").unwrap()];
        // 16 GB card: everything on the GPU, no RAM needed
        let full = plan(&clef, Accelerator::Cuda, 14632, 20000, &HashMap::new()).unwrap();
        assert!(!full[0].profile.offloaded(), "{}", full[0].profile.id);
        // 8 GB card with plenty of RAM: offload, the best context that fits
        let eight = plan(&clef, Accelerator::Cuda, 7800, 20000, &HashMap::new()).unwrap();
        assert!(eight[0].profile.offloaded(), "{}", eight[0].profile.id);
        assert!(eight[0].profile.memory_mib <= 7800);
        // 8 GB card without RAM to spare: nothing fits
        assert!(plan(&clef, Accelerator::Cuda, 7800, 1000, &HashMap::new()).is_err());
    }

    #[test]
    fn a_4_gb_laptop_gpu_with_the_desktop_on_it_gets_a_short_context() {
        // RTX 3050 Ti Laptop: 1784 MiB free, 18.5 GB of RAM available
        let catalog = catalog();
        let clef = [catalog.model("clef").unwrap()];
        let choices = plan(&clef, Accelerator::Cuda, 1784 - 256, 18551 - 2048, &HashMap::new()).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-2048-gpu0");
        assert_eq!(choices[0].placement, Placement::Published);
    }

    #[test]
    fn h2o_on_a_4_gb_gpu_with_2_5_gb_free_uses_ram() {
        let catalog = catalog();
        let h2o = catalog.model("h2o-q8").unwrap();
        let choices = plan(&[h2o], Accelerator::Cuda, 2560 - 256, 19000, &HashMap::new()).unwrap();
        let choice = &choices[0];
        assert_eq!(choice.profile.id, "cuda-8192");
        assert_eq!(choice.profile.context_tokens, 8192);
        assert_eq!(choice.profile.slots(), 1);
        assert_eq!(choice.placement, Placement::Ram { memory_mib: 1280, host_memory_mib: 7168 });
        assert_eq!(choice.placement.gpu_layers(choice.profile, h2o.execution.gpu_layers), 0);
        assert_eq!(choice.profile.memory_mib, 5120);
        assert_eq!(choice.profile.gpu_layers, None);
        assert!(std::ptr::eq(choice.profile, h2o.profile("cuda-8192").unwrap()));
        crate::setup::print_plan(&choices);
    }

    #[test]
    fn ram_fallback_needs_both_memory_budgets() {
        let catalog = catalog();
        let h2o = [catalog.model("h2o-q8").unwrap()];
        assert!(plan(&h2o, Accelerator::Cuda, 1280, 7168, &HashMap::new()).is_ok());
        assert!(plan(&h2o, Accelerator::Cuda, 1279, 7168, &HashMap::new()).is_err());
        assert!(plan(&h2o, Accelerator::Cuda, 1280, 7167, &HashMap::new()).is_err());
        let full = plan(&h2o, Accelerator::Cuda, 9216, 20000, &HashMap::new()).unwrap();
        assert_eq!(full[0].placement, Placement::Published);
    }

    #[test]
    fn forced_profiles_and_contexts_keep_their_setting_in_ram() {
        let catalog = catalog();
        let h2o = [catalog.model("h2o-q8").unwrap()];
        let overrides = parse_overrides(&["h2o-q8=cuda-40960-x3".into()], &[]).unwrap();
        let forced = plan(&h2o, Accelerator::Cuda, 6000, 20000, &overrides).unwrap();
        assert_eq!(forced[0].profile.id, "cuda-40960-x3");
        assert_eq!(forced[0].profile.slots(), 3);
        assert_eq!(forced[0].placement, Placement::Ram { memory_mib: 5197, host_memory_mib: 11264 });
        assert!(plan(&h2o, Accelerator::Cuda, 2304, 20000, &overrides).is_err());

        let overrides = parse_overrides(&[], &["h2o-q8=40960".into()]).unwrap();
        let forced = plan(&h2o, Accelerator::Cuda, 10000, 20000, &overrides).unwrap();
        assert_eq!(forced[0].profile.context_tokens, 40960);
        assert_eq!(forced[0].profile.id, "cuda-40960-x6");
        assert_ne!(forced[0].placement, Placement::Published);

        let automatic = plan(&h2o, Accelerator::Cuda, 6000, 20000, &HashMap::new()).unwrap();
        assert_eq!(automatic[0].profile.id, "cuda-8192");
        assert_eq!(automatic[0].profile.slots(), 1);
    }

    #[test]
    fn combined_pressure_does_not_trigger_ram_fallback() {
        let catalog = catalog();
        let mut second = catalog.model("h2o-q8").unwrap().clone();
        second.id = "second".into();
        let h2o = catalog.model("h2o-q8").unwrap();
        let error = plan(&[h2o, &second], Accelerator::Cuda, 10000, 40000, &HashMap::new()).unwrap_err();
        assert!(error.contains("do not fit"), "{error}");
        assert!(!error.contains("automatic RAM fallback"), "{error}");

        // Both qualify individually for RAM, but their host reservations add up.
        assert!(plan(&[h2o, &second], Accelerator::Cuda, 3000, 14000, &HashMap::new()).is_err());
        let choices = plan(&[h2o, &second], Accelerator::Cuda, 3000, 15000, &HashMap::new()).unwrap();
        assert!(choices.iter().all(|choice| choice.placement != Placement::Published));
    }

    #[test]
    fn ram_fallback_is_cuda_only_and_supports_chat() {
        let catalog = catalog();
        let chat = catalog.model("qwen2.5-1.5b-instruct").unwrap();
        let choices = plan(&[chat], Accelerator::Cuda, 1280, 10000, &HashMap::new()).unwrap();
        assert_eq!(choices[0].profile.id, "cuda-4096");
        assert_ne!(choices[0].placement, Placement::Published);
        assert!(plan(&[chat], Accelerator::Metal, 1280, 10000, &HashMap::new()).is_err());
        assert!(ram_fallback(chat, chat.profile("metal-4096").unwrap()).is_none());
    }

    #[test]
    fn ram_estimates_do_not_undercut_published_zero_layer_requirements() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let profile = clef.profile("cuda-16384").unwrap();
        let ram = ram_fallback(clef, profile).unwrap();
        let published = clef.profile("cuda-16384-gpu0").unwrap();
        assert!(ram.memory_mib(profile) >= published.memory_mib);
        assert!(ram.host_memory_mib(profile) >= published.host_memory_mib.unwrap());

        let mut large = clef.clone();
        large.weights.size_bytes = u64::MAX;
        assert!(ram_fallback(&large, profile).unwrap().host_memory_mib(profile) > 1 << 40);
    }

    #[test]
    fn benchmarks_include_all_fitting_placements_and_respect_filters() {
        let catalog = catalog();
        let clef = catalog.model("clef").unwrap();
        let all = benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, None).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|choice| !choice.profile.offloaded()));
        let offload = benchmark_choices(clef, Accelerator::Cuda, 7800, 40000, None).unwrap();
        assert!(!offload.is_empty());
        assert!(offload.iter().all(|choice| choice.profile.offloaded()));
        assert_eq!(
            offload.len(),
            clef.profiles_for(Accelerator::Cuda).iter().filter(|profile| profile.memory_mib <= 7800).count()
        );
        let selected = benchmark_choices(
            clef,
            Accelerator::Cuda,
            20000,
            40000,
            Some(&Override::Profile("cuda-4096-gpu16".into())),
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].profile.id, "cuda-4096-gpu16");
        let short = benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, Some(&Override::Context(4096))).unwrap();
        assert_eq!(short.len(), 5);
        assert!(short.iter().all(|choice| choice.profile.context_tokens == 4096));
        let full = benchmark_choices(clef, Accelerator::Cuda, 20000, 0, None).unwrap();
        assert_eq!(full.len(), 3);
        assert!(full.iter().all(|choice| !choice.profile.offloaded()));
        assert!(benchmark_choices(clef, Accelerator::Cuda, 20000, 40000, Some(&Override::Context(5000))).is_err());

        let h2o = catalog.model("h2o-q8").unwrap();
        let ram = benchmark_choices(h2o, Accelerator::Cuda, 2304, 19000, None).unwrap();
        assert_eq!(ram.len(), 1);
        assert_eq!(ram[0].profile.id, "cuda-8192");
        assert_ne!(ram[0].placement, Placement::Published);
        let slots = benchmark_choices(h2o, Accelerator::Cuda, 15000, 19000, Some(&Override::Context(40960))).unwrap();
        assert_eq!(slots.iter().map(|choice| choice.profile.slots()).collect::<Vec<_>>(), [3, 4, 6]);
        let forced =
            benchmark_choices(h2o, Accelerator::Cuda, 6000, 19000, Some(&Override::Profile("cuda-40960-x3".into())))
                .unwrap();
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].profile.id, "cuda-40960-x3");
        assert_ne!(forced[0].placement, Placement::Published);
    }
}
