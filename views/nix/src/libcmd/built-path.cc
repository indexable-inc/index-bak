#include "nix/cmd/built-path.hh"
#include "nix/store/derivations.hh"
#include "nix/store/store-api.hh"
#include "nix/util/comparator.hh"

#include <nlohmann/json.hpp>

#include <optional>

namespace nix {

// Custom implementation to avoid `ref` ptr equality
GENERATE_CMP_EXT(, std::strong_ordering, SingleBuiltPathBuilt, *me->drvPath, me->output);

// Custom implementation to avoid `ref` ptr equality

// TODO no `GENERATE_CMP_EXT` because no `std::set::operator<=>` on
// Darwin, per header.
GENERATE_EQUAL(, BuiltPathBuilt ::, BuiltPathBuilt, *me->drvPath, me->outputs);

StorePath SingleBuiltPath::outPath() const
{
    return std::visit(
        overloaded{
            [](const SingleBuiltPath::Opaque & p) { return p.path; },
            [](const SingleBuiltPath::Built & b) { return b.output.second; },
        },
        raw());
}

StorePathSet BuiltPath::outPaths() const
{
    return std::visit(
        overloaded{
            [](const BuiltPath::Opaque & p) { return StorePathSet{p.path}; },
            [](const BuiltPath::Built & b) {
                StorePathSet res;
                for (auto & [_, path] : b.outputs)
                    res.insert(path);
                return res;
            },
        },
        raw());
}

SingleDerivedPath::Built SingleBuiltPath::Built::discardOutputPath() const
{
    return SingleDerivedPath::Built{
        .drvPath = make_ref<SingleDerivedPath>(drvPath->discardOutputPath()),
        .output = output.first,
    };
}

SingleDerivedPath SingleBuiltPath::discardOutputPath() const
{
    return std::visit(
        overloaded{
            [](const SingleBuiltPath::Opaque & p) -> SingleDerivedPath { return p; },
            [](const SingleBuiltPath::Built & b) -> SingleDerivedPath { return b.discardOutputPath(); },
        },
        raw());
}

nlohmann::json BuiltPath::Built::toJSON(const StoreDirConfig & store) const
{
    nlohmann::json res;
    res["drvPath"] = drvPath->toJSON(store);
    for (const auto & [outputName, outputPath] : outputs) {
        res["outputs"][outputName] = store.printStorePath(outputPath);
    }
    return res;
}

nlohmann::json SingleBuiltPath::Built::toJSON(const StoreDirConfig & store) const
{
    nlohmann::json res;
    res["drvPath"] = drvPath->toJSON(store);
    auto & [outputName, outputPath] = output;
    res["output"] = outputName;
    res["outputPath"] = store.printStorePath(outputPath);
    return res;
}

nlohmann::json SingleBuiltPath::toJSON(const StoreDirConfig & store) const
{
    return std::visit(
        overloaded{
            [&](const SingleBuiltPath::Opaque & o) -> nlohmann::json { return store.printStorePath(o.path); },
            [&](const SingleBuiltPath::Built & b) { return b.toJSON(store); },
        },
        raw());
}

nlohmann::json BuiltPath::toJSON(const StoreDirConfig & store) const
{
    return std::visit(
        overloaded{
            [&](const BuiltPath::Opaque & o) -> nlohmann::json { return store.printStorePath(o.path); },
            [&](const BuiltPath::Built & b) { return b.toJSON(store); },
        },
        raw());
}

RealisedPath::Set BuiltPath::toRealisedPaths(Store & store, RealisationScope scope) const
{
    RealisedPath::Set res;
    std::set<DrvOutput> copied;
    std::set<DrvOutput> visiting;
    StorePathSet proofPaths;
    std::function<void(const StorePath &, const OutputName &, const StorePath &)> addOutput;
    std::function<void(const StorePath &, const DerivedPathMap<StringSet>::ChildNode &)> addInputs;
    addInputs = [&](const StorePath & drvPath, const auto & node) {
        auto drv = store.readDerivation(drvPath);
        auto outputs = store.queryPartialDerivationOutputMap(drvPath);
        auto requireOutput = [&](const OutputName & name) {
            auto output = outputs.find(name);
            if (output == outputs.end() || !output->second)
                throw Error("missing current realization for '%s^%s'", store.printStorePath(drvPath), name);
            return *output->second;
        };
        for (const auto & name : node.value)
            // Static input paths are recoverable from the derivation alone;
            // their build graphs and realization rows are not proof inputs.
            if (!drv.outputs.at(name).path(store, drv.name, name)) {
                auto path = requireOutput(name);
                addOutput(drvPath, name, path);
                proofPaths.insert(path);
            }
        for (const auto & [name, child] : node.childMap) {
            auto path = requireOutput(name);
            addOutput(drvPath, name, path);
            proofPaths.insert(path);
            addInputs(path, child);
        }
    };
    addOutput = [&](const StorePath & drvPath, const OutputName & outputName, const StorePath & outputPath) {
        auto drv = store.readDerivation(drvPath);
        DrvOutput key{staticOutputHashes(store, drv).at(outputName), outputName};
        if (copied.contains(key))
            return;
        if (!visiting.insert(key).second)
            throw Error("cycle while copying realization '%s'", key.to_string());
        auto realization = store.queryRealisation(key);
        if (!realization || realization->outPath != outputPath)
            throw Error("missing or stale realization '%s' while copying", key.to_string());
        if (scope == RealisationScope::Outputs) {
            res.insert(Realisation{*realization, key});
            visiting.erase(key);
            copied.insert(key);
            return;
        }
        // A recipient needs the input mappings and normalized CA witness to
        // validate a cache hit. Copy their existing signatures, never infer or
        // sign a normalized mapping from the unresolved alias alone.
        if (std::holds_alternative<DerivationOutput::Deferred>(drv.outputs.at(outputName).raw)
            || std::holds_alternative<DerivationOutput::CAFloating>(drv.outputs.at(outputName).raw))
            for (const auto & [input, node] : drv.inputDrvs.map)
                addInputs(input, node);
        if (!drv.inputDrvs.map.empty()
            && std::holds_alternative<DerivationOutput::CAFloating>(drv.outputs.at(outputName).raw)) {
            auto resolved = drv.tryResolve(store);
            if (!resolved)
                throw Error("cannot resolve current inputs for realization '%s'", key.to_string());
            DrvOutput normalized{staticOutputHashes(store, Derivation{*resolved}).at(outputName), outputName};
            auto witness = store.queryRealisation(normalized);
            if (!witness || witness->outPath != outputPath)
                throw Error("missing or incorrect normalized realization '%s'", normalized.to_string());
            res.insert(Realisation{*witness, normalized});
        }
        res.insert(Realisation{*realization, key});
        visiting.erase(key);
        copied.insert(key);
    };
    std::visit(
        overloaded{
            [&](const BuiltPath::Opaque & p) { res.insert(p.path); },
            [&](const BuiltPath::Built & p) {
                for (auto & [outputName, outputPath] : p.outputs) {
                    if (experimentalFeatureSettings.isEnabled(Xp::CaDerivations)) {
                        addOutput(p.drvPath->outPath(), outputName, outputPath);
                    } else {
                        res.insert(outputPath);
                    }
                }
            },
        },
        raw());
    StorePathSet proofClosure;
    store.computeFSClosure(proofPaths, proofClosure);
    for (const auto & path : proofClosure)
        res.insert(path);
    return res;
}

} // namespace nix
