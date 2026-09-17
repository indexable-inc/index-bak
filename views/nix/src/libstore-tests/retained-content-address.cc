#include <gtest/gtest.h>
#include <stdexcept>

#include "nix/store/local-store.hh"
#include "nix/store/build/worker.hh"
#include "nix/store/derivations.hh"
#include "nix/store/downstream-placeholder.hh"
#include "nix/store/globals.hh"
#include "nix/store/local-settings.hh"
#include "nix/store/store-open.hh"
#include "nix/util/file-system.hh"
#include "nix/util/callback.hh"
#include "nix/util/archive.hh"
#include "nix/util/finally.hh"
#include "nix/util/posix-source-accessor.hh"

#ifndef _WIN32

#  include "nix/store/build/worker.hh"
#  include "nix/store/globals.hh"
#  include "nix/store/store-open.hh"
#  include "nix/util/file-system.hh"
#  include "nix/util/posix-source-accessor.hh"

namespace nix {

class RetainedContentAddressTest : public ::testing::Test
{
protected:
    std::filesystem::path root;
    std::shared_ptr<LocalStore> store;

    void SetUp() override
    {
        experimentalFeatureSettings.set("extra-experimental-features", "ca-derivations");
        root = createTempDir();
        store = openStore(
                    "local",
                    {{"store", (root / "store").string()},
                     {"state", (root / "state").string()},
                     {"log", (root / "log").string()}})
                    .dynamic_pointer_cast<LocalStore>();
        ASSERT_TRUE(store);
    }

    void TearDown() override
    {
        store.reset();
        std::filesystem::remove_all(root);
    }

    StorePath record(const std::string & contents, bool invalidAddress, bool dangling = false)
    {
        auto temporary = root / "payload";
        if (dangling)
            std::filesystem::create_symlink(contents, temporary);
        else
            writeFile(temporary, contents);
        auto nar =
            hashPath(makeFSSourceAccessor(temporary), FileIngestionMethod::NixArchive, HashAlgorithm::SHA256).first;
        auto address = invalidAddress ? hashString(HashAlgorithm::SHA256, "historical producer identity") : nar;
        auto path = store->makeFixedOutputPath(
            "retained-output", {.method = FileIngestionMethod::NixArchive, .hash = address, .references = {}});
        std::filesystem::rename(temporary, store->toRealPath(path));
        ValidPathInfo info{path, UnkeyedValidPathInfo(*store, nar)};
        info.ca = ContentAddress{.method = ContentAddressMethod::Raw::NixArchive, .hash = address};
        // A former producer admitted its own output with an intact NAR but an
        // incorrect CA. Import through the corrected reader would reject it.
        store->registerValidPaths({{path, info}});
        return path;
    }

    DrvOutput identity()
    {
        return {.drvHash = hashString(HashAlgorithm::SHA256, "retained derivation"), .outputName = "out"};
    }

    void registerOutput(const StorePath & path)
    {
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = path}, identity()});
    }

    void proveBuildRepair(BuildMode mode, bool invalidAddress = true)
    {
        auto & sandboxMode = settings.getLocalSettings().sandboxMode;
        auto previousSandboxMode = sandboxMode.get();
        sandboxMode = smDisabled;
        Finally restoreSandbox = [&]() { sandboxMode = previousSandboxMode; };
        Derivation drv;
        drv.name = "retained-output";
        drv.platform = settings.thisSystem.get();
        drv.builder = "/bin/sh";
        // A real installer refuses an existing output. Requiring absence also
        // catches an in-place overwrite that identical payload bytes would hide.
        drv.args = {"-c", "test ! -e \"$out\" && printf 'same payload' > \"$out\""};
        drv.env = {{"name", drv.name}, {"out", hashPlaceholder("out")}, {"system", drv.platform}};
        drv.outputs.emplace(
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive, .hashAlgo = HashAlgorithm::SHA256}});
        auto drvPath = store->writeDerivation(drv);
        DrvOutput outputId{.drvHash = staticOutputHashes(*store, drv).at("out"), .outputName = "out"};
        auto invalid = record("same payload", invalidAddress);
        if (!invalidAddress)
            writeFile(store->toRealPath(invalid), "damaged archive");
        auto historical = lstat(store->toRealPath(invalid));
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId});
        store->buildPaths(
            {DerivedPath::Built{.drvPath = makeConstantStorePathRef(drvPath), .outputs = OutputsSpec::Names{"out"}}},
            mode);
        auto repaired = store->queryRealisation(outputId);
        ASSERT_TRUE(repaired);
        if (invalidAddress) {
            EXPECT_NE(repaired->outPath, invalid);
        } else {
            EXPECT_EQ(repaired->outPath, invalid);
        }
        EXPECT_EQ(readFile(store->toRealPath(repaired->outPath)), "same payload");
        EXPECT_EQ(readFile(store->toRealPath(invalid)), "same payload");
        if (invalidAddress) {
            EXPECT_EQ(lstat(store->toRealPath(invalid)).st_ino, historical.st_ino);
        }
        Worker worker{*store, *store};
        EXPECT_EQ(worker.checkPathContents(repaired->outPath), PathContentStatus::Valid);
        EXPECT_EQ(
            worker.checkPathContents(invalid),
            invalidAddress ? PathContentStatus::InvalidContentAddress : PathContentStatus::Valid);
    }
};

class DeferredAliasTest : public RetainedContentAddressTest
{
protected:
    Derivation producer()
    {
        Derivation drv;
        drv.name = "producer";
        drv.platform = settings.thisSystem.get();
        drv.builder = "/bin/sh";
        drv.args = {"-c", "printf payload > \"$out\""};
        drv.env = {{"name", drv.name}, {"out", hashPlaceholder("out")}, {"system", drv.platform}};
        drv.outputs.emplace(
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive, .hashAlgo = HashAlgorithm::SHA256}});
        return drv;
    }

    Derivation consumer(const StorePath & dependency, const std::string & name)
    {
        Derivation drv;
        drv.name = name;
        drv.platform = settings.thisSystem.get();
        drv.builder = "/bin/sh";
        auto input = DownstreamPlaceholder::unknownCaOutput(dependency, "out").render();
        drv.args = {"-c", "printf '%s' '" + input + "' > \"$out\""};
        drv.env = {{"name", name}, {"out", ""}, {"system", drv.platform}};
        drv.outputs.emplace("out", DerivationOutput{DerivationOutput::Deferred{}});
        drv.inputDrvs.map[dependency].value.insert("out");
        return drv;
    }

    DrvOutput outputId(const Derivation & drv)
    {
        return {staticOutputHashes(*store, drv).at("out"), "out"};
    }

    StorePath historicalConsumer(const Derivation & drv, const StorePath & dependency)
    {
        auto resolved = drv.tryResolve(*store);
        if (!resolved)
            throw Error("fixture consumer must resolve");
        auto output = resolved->outputs.at("out").path(*store, drv.name, "out");
        if (!output)
            throw Error("fixture consumer must be input addressed after resolution");
        writeFile(store->toRealPath(*output), store->printStorePath(dependency));
        auto nar = hashPath(
                       makeFSSourceAccessor(store->toRealPath(*output)),
                       FileIngestionMethod::NixArchive,
                       HashAlgorithm::SHA256)
                       .first;
        ValidPathInfo info{*output, UnkeyedValidPathInfo(*store, nar)};
        store->registerValidPaths({{*output, info}});
        // Historical aliases did not record their resolved input identities.
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = *output}, outputId(drv)});
        return *output;
    }

    void build(const StorePath & drv)
    {
        store->buildPaths(
            {DerivedPath::Built{.drvPath = makeConstantStorePathRef(drv), .outputs = OutputsSpec::Names{"out"}}});
    }
};

class FloatingAliasTest : public DeferredAliasTest
{
protected:
    Derivation floatingConsumer(const StorePath & dependency, const std::string & name)
    {
        auto drv = consumer(dependency, name);
        drv.env["out"] = hashPlaceholder("out");
        drv.outputs.at("out") = DerivationOutput{DerivationOutput::CAFloating{
            .method = ContentAddressMethod::Raw::NixArchive, .hashAlgo = HashAlgorithm::SHA256}};
        return drv;
    }

    StorePath historicalFloatingConsumer(
        const Derivation & drv,
        const StorePath & dependency,
        bool normalizedWitness = true,
        Store * evalStore = nullptr)
    {
        auto resolved = drv.tryResolve(*store, evalStore);
        if (!resolved)
            throw Error("fixture floating consumer must resolve");
        auto temporary = root / "historical-consumer";
        writeFile(temporary, store->printStorePath(dependency));
        auto nar =
            hashPath(makeFSSourceAccessor(temporary), FileIngestionMethod::NixArchive, HashAlgorithm::SHA256).first;
        auto path = store->makeFixedOutputPath(
            drv.name,
            {.method = FileIngestionMethod::NixArchive,
             .hash = nar,
             .references = {.others = {dependency}, .self = false}});
        std::filesystem::rename(temporary, store->toRealPath(path));
        ValidPathInfo info{path, UnkeyedValidPathInfo(*store, nar)};
        info.ca = ContentAddress{.method = ContentAddressMethod::Raw::NixArchive, .hash = nar};
        info.references.insert(dependency);
        store->registerValidPaths({{path, info}});
        if (normalizedWitness)
            store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = path}, outputId(Derivation{*resolved})});
        DrvOutput original{staticOutputHashes(evalStore ? *evalStore : *store, drv).at("out"), "out"};
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = path}, original});
        return path;
    }
};

TEST_F(FloatingAliasTest, RepairsDirectAndTransitiveAliasesAndReusesCurrentProof)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    auto direct = floatingConsumer(rootPath, "floating-direct");
    auto directPath = store->writeDerivation(direct);
    auto oldDirect = historicalFloatingConsumer(direct, invalid);
    auto transitive = floatingConsumer(directPath, "floating-transitive");
    auto transitivePath = store->writeDerivation(transitive);
    auto oldTransitive = historicalFloatingConsumer(transitive, oldDirect);
    auto oldDirectStat = lstat(store->toRealPath(oldDirect));
    auto oldTransitiveStat = lstat(store->toRealPath(oldTransitive));
    Worker oldWorker{*store, *store};
    ASSERT_EQ(oldWorker.checkPathContents(oldDirect), PathContentStatus::Valid);
    ASSERT_EQ(oldWorker.checkPathContents(oldTransitive), PathContentStatus::Valid);

    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    EXPECT_FALSE(store->queryPartialDerivationOutputMap(directPath).at("out"));
    EXPECT_FALSE(store->queryPartialDerivationOutputMap(transitivePath).at("out"));
    EXPECT_THROW(
        store->registerResolvedDrvOutput(
            Realisation{UnkeyedRealisation{.outPath = oldDirect}, outputId(direct)}, direct, *store),
        Error);
    build(transitivePath);
    auto currentDirect = store->queryPartialDerivationOutputMap(directPath).at("out");
    auto currentTransitive = store->queryPartialDerivationOutputMap(transitivePath).at("out");
    ASSERT_TRUE(currentDirect);
    ASSERT_TRUE(currentTransitive);
    EXPECT_NE(*currentDirect, oldDirect);
    EXPECT_NE(*currentTransitive, oldTransitive);
    EXPECT_EQ(readFile(store->toRealPath(*currentDirect)), store->printStorePath(valid));
    EXPECT_EQ(readFile(store->toRealPath(*currentTransitive)), store->printStorePath(*currentDirect));
    EXPECT_EQ(readFile(store->toRealPath(oldDirect)), store->printStorePath(invalid));
    EXPECT_EQ(readFile(store->toRealPath(oldTransitive)), store->printStorePath(oldDirect));
    EXPECT_EQ(lstat(store->toRealPath(oldDirect)).st_ino, oldDirectStat.st_ino);
    EXPECT_EQ(lstat(store->toRealPath(oldTransitive)).st_ino, oldTransitiveStat.st_ino);
    auto results = store->buildPathsWithResults({DerivedPath::Built{
        .drvPath = makeConstantStorePathRef(transitivePath), .outputs = OutputsSpec::Names{"out"}}});
    ASSERT_EQ(results.size(), size_t{1});
    auto success = results.front().tryGetSuccess();
    ASSERT_TRUE(success);
    EXPECT_EQ(success->status, BuildResult::Success::AlreadyValid);
}

TEST_F(FloatingAliasTest, ReresolvesCachedChildAfterSharedProducerRepairInOneBuild)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto historicalRoot = lstat(store->toRealPath(invalid));
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});

    // Goal keys order this cache hit before the producer goal. The parent
    // also requests the producer directly, which discovers its invalid CA
    // and repairs it while the already-successful child result is retained.
    auto child = floatingConsumer(rootPath, "a-cached-child");
    auto childPath = store->writeDerivation(child);
    auto historicalChild = historicalFloatingConsumer(child, invalid);
    auto historicalChildStat = lstat(store->toRealPath(historicalChild));
    auto parent = floatingConsumer(childPath, "parent");
    parent.inputDrvs.map[rootPath].value.insert("out");
    auto parentPath = store->writeDerivation(parent);

    build(parentPath);

    auto repairedRoot = store->queryRealisation(outputId(rootDrv));
    ASSERT_TRUE(repairedRoot);
    EXPECT_NE(repairedRoot->outPath, invalid);
    auto currentChild = store->queryPartialDerivationOutputMap(childPath).at("out");
    auto currentParent = store->queryPartialDerivationOutputMap(parentPath).at("out");
    ASSERT_TRUE(currentChild);
    ASSERT_TRUE(currentParent);
    EXPECT_NE(*currentChild, historicalChild);
    EXPECT_EQ(readFile(store->toRealPath(*currentChild)), store->printStorePath(repairedRoot->outPath));
    EXPECT_EQ(readFile(store->toRealPath(*currentParent)), store->printStorePath(*currentChild));
    EXPECT_EQ(store->queryRealisation(outputId(child))->outPath, *currentChild);
    EXPECT_EQ(store->queryRealisation(outputId(parent))->outPath, *currentParent);
    EXPECT_EQ(readFile(store->toRealPath(historicalChild)), store->printStorePath(invalid));
    EXPECT_EQ(lstat(store->toRealPath(historicalChild)).st_ino, historicalChildStat.st_ino);
    EXPECT_EQ(lstat(store->toRealPath(invalid)).st_ino, historicalRoot.st_ino);
}

TEST_F(FloatingAliasTest, MissingNormalizedWitnessDoesNotAuthorizeStaticAlias)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    auto direct = floatingConsumer(rootPath, "missing-witness");
    auto path = store->writeDerivation(direct);
    auto historical = historicalFloatingConsumer(direct, valid, false);
    EXPECT_FALSE(store->queryPartialDerivationOutputMap(path).at("out"));
    EXPECT_THROW(
        store->registerResolvedDrvOutput(
            Realisation{UnkeyedRealisation{.outPath = historical}, outputId(direct)}, direct, *store),
        Error);
    build(path);
    EXPECT_EQ(store->queryPartialDerivationOutputMap(path).at("out"), historical);
}

TEST_F(FloatingAliasTest, NormalizedIdentityStillRejectsNondeterminismAndWrongProof)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    auto direct = floatingConsumer(rootPath, "normalized-identity");
    auto path = store->writeDerivation(direct);
    build(path);
    auto resolved = direct.tryResolve(*store);
    ASSERT_TRUE(resolved);
    auto normalized = outputId(Derivation{*resolved});
    auto original = store->queryRealisation(normalized);
    ASSERT_TRUE(original);
    auto different = record("different valid payload", false);
    EXPECT_THROW(store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = different}, normalized}), Error);
    EXPECT_THROW(
        store->registerResolvedDrvOutput(
            Realisation{UnkeyedRealisation{.outPath = different}, outputId(direct)}, direct, *store),
        Error);
    EXPECT_EQ(store->queryRealisation(normalized)->outPath, original->outPath);
    EXPECT_EQ(store->queryRealisation(outputId(direct))->outPath, original->outPath);
}

TEST_F(FloatingAliasTest, EvaluatorOwnsDerivationsWhileBuildStoreOwnsCurrentProof)
{
    auto evaluator = openStore(
        "local",
        {{"store", store->storeDir}, {"state", (root / "eval-state").string()}, {"log", (root / "eval-log").string()}});
    auto rootDrv = producer();
    auto rootPath = evaluator->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    auto producerId = outputId(rootDrv);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, producerId});
    auto direct = floatingConsumer(rootPath, "separate-evaluator");
    auto directPath = evaluator->writeDerivation(direct);
    auto oldOutput = historicalFloatingConsumer(direct, invalid, true, &*evaluator);
    ASSERT_TRUE(evaluator->isValidPath(rootPath));
    ASSERT_FALSE(store->isValidPath(rootPath));
    ASSERT_FALSE(evaluator->queryRealisation(producerId));
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, producerId});
    auto results = store->buildPathsWithResults(
        {DerivedPath::Built{.drvPath = makeConstantStorePathRef(directPath), .outputs = OutputsSpec::Names{"out"}}},
        bmNormal,
        evaluator.get_ptr());
    ASSERT_EQ(results.size(), size_t{1});
    ASSERT_TRUE(results.front().tryGetSuccess());
    auto current = store->queryPartialDerivationOutputMap(directPath, &*evaluator).at("out");
    ASSERT_TRUE(current);
    EXPECT_NE(*current, oldOutput);
    EXPECT_EQ(readFile(store->toRealPath(*current)), store->printStorePath(valid));
    EXPECT_EQ(readFile(store->toRealPath(oldOutput)), store->printStorePath(invalid));
    EXPECT_FALSE(evaluator->queryRealisation(producerId));
}

TEST_F(RetainedContentAddressTest, BinaryCachePreservesOnlySameOutputSignatures)
{
    auto cache = openStore("file://" + (root / "binary-cache").string());
    auto first = record("first valid payload", false);
    auto second = record("second valid payload", false);
    LocalSigner signer{SecretKey::generate("cache-writer")};
    Realisation signedOutput{UnkeyedRealisation{.outPath = first}, identity()};
    signedOutput.sign(signedOutput.id, signer);
    cache->registerDrvOutput(signedOutput, NoCheckSigs);
    cache->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = first}, identity()}, NoCheckSigs);
    auto repeated = cache->queryRealisation(identity());
    ASSERT_TRUE(repeated);
    EXPECT_EQ(repeated->signatures, signedOutput.signatures);
    cache->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = second}, identity()}, NoCheckSigs);
    auto changed = cache->queryRealisation(identity());
    ASSERT_TRUE(changed);
    EXPECT_EQ(changed->outPath, second);
    EXPECT_TRUE(changed->signatures.empty());
    auto otherIdentity = identity();
    otherIdentity.outputName = "other";
    cache->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = first}, otherIdentity}, NoCheckSigs);
    EXPECT_TRUE(cache->queryRealisation(otherIdentity)->signatures.empty());
}

class CountingLocalStore : public LocalStore
{
public:
    explicit CountingLocalStore(ref<const Config> config)
        : Store{*config}
        , LocalFSStore{*config}
        , LocalStore{config}
    {
    }

    size_t derivationReads = 0;
    std::optional<StorePath> throwOnRead;

    Derivation readInvalidDerivation(const StorePath & path) override
    {
        ++derivationReads;
        if (throwOnRead == path)
            throw std::runtime_error("injected derivation read failure");
        return LocalStore::readInvalidDerivation(path);
    }
};

class InterleavingLocalStore : public CountingLocalStore
{
public:
    explicit InterleavingLocalStore(ref<const Config> config)
        : Store{*config}
        , LocalFSStore{*config}
        , CountingLocalStore{config}
    {
    }

    std::optional<DrvOutput> target;
    std::function<void()> afterRead;

    void queryRealisationUncached(
        const DrvOutput & id, Callback<std::shared_ptr<const UnkeyedRealisation>> callback) noexcept override
    {
        auto receiver = std::make_shared<decltype(callback)>(std::move(callback));
        LocalStore::queryRealisationUncached(id, {[this, id, receiver](auto future) {
                                                 try {
                                                     auto result = future.get();
                                                     if (target && id == *target && afterRead) {
                                                         auto action = std::exchange(afterRead, {});
                                                         action();
                                                     }
                                                     (*receiver)(std::move(result));
                                                 } catch (...) {
                                                     receiver->rethrow();
                                                 }
                                             }});
    }
};

TEST_F(DeferredAliasTest, ConcurrentWinnerIsAcceptedAndOnlyCurrentSignaturesMerge)
{
    // Replace the empty fixture store before it has acquired process roots.
    auto interleaving = std::make_shared<InterleavingLocalStore>(store->config);
    store = interleaving;
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    auto direct = consumer(rootPath, "direct");
    store->writeDerivation(direct);
    auto oldOutput = historicalConsumer(direct, invalid);
    auto id = outputId(direct);
    LocalSigner historicalSigner{SecretKey::generate("historical")};
    Realisation historical{UnkeyedRealisation{.outPath = oldOutput}, id};
    historical.sign(id, historicalSigner);
    store->registerDrvOutput(historical);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});

    auto resolved = direct.tryResolve(*store);
    ASSERT_TRUE(resolved);
    auto output = resolved->outputs.at("out").path(*store, direct.name, "out");
    ASSERT_TRUE(output);
    writeFile(store->toRealPath(*output), store->printStorePath(valid));
    auto nar =
        hashPath(
            makeFSSourceAccessor(store->toRealPath(*output)), FileIngestionMethod::NixArchive, HashAlgorithm::SHA256)
            .first;
    ValidPathInfo pathInfo{*output, UnkeyedValidPathInfo(*store, nar)};
    store->registerValidPaths({{*output, pathInfo}});
    LocalSigner winnerSigner{SecretKey::generate("winner")};
    LocalSigner followerSigner{SecretKey::generate("follower")};
    Realisation winner{UnkeyedRealisation{.outPath = *output}, id};
    Realisation follower{UnkeyedRealisation{.outPath = *output}, id};
    winner.sign(id, winnerSigner);
    follower.sign(id, followerSigner);
    interleaving->target = id;
    interleaving->afterRead = [&]() {
        // Publish after the follower reads the historical row, but before it
        // opens its proof transaction: the interleaving observed with real RPC.
        store->registerResolvedDrvOutput(winner, direct, *store);
    };
    EXPECT_NO_THROW(store->registerResolvedDrvOutput(follower, direct, *store));
    EXPECT_FALSE(interleaving->afterRead);
    auto current = store->queryRealisation(id);
    ASSERT_TRUE(current);
    EXPECT_EQ(current->outPath, *output);
    auto signatures = winner.signatures;
    signatures.insert(follower.signatures.begin(), follower.signatures.end());
    EXPECT_EQ(current->signatures, signatures);
    EXPECT_EQ(readFile(store->toRealPath(oldOutput)), store->printStorePath(invalid));
}

TEST_F(DeferredAliasTest, SharedDependencyDagHasLinearReadsAndMemoExpires)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    auto previous = rootPath;
    auto preceding = rootPath;
    constexpr size_t depth = 18;
    for (size_t level = 0; level < depth; ++level) {
        auto drv = consumer(previous, "shared-" + std::to_string(level));
        drv.inputDrvs.map[preceding].value.insert("out");
        auto next = store->writeDerivation(drv);
        preceding = previous;
        previous = next;
    }
    CountingLocalStore reader{store->config};
    auto oldOutput = reader.queryPartialDerivationOutputMap(previous).at("out");
    ASSERT_TRUE(oldOutput);
    EXPECT_LE(reader.derivationReads, 8 * (depth + 1));
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    reader.derivationReads = 0;
    auto newOutput = reader.queryPartialDerivationOutputMap(previous).at("out");
    ASSERT_TRUE(newOutput);
    EXPECT_NE(oldOutput, newOutput);
    EXPECT_LE(reader.derivationReads, 8 * (depth + 1));
}

TEST_F(DeferredAliasTest, RootResolutionSharesSiblingProofsAndExpiresAfterException)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    constexpr size_t depth = 32;
    constexpr size_t width = 32;
    auto shared = rootPath;
    for (size_t level = 0; level < depth; ++level)
        shared = store->writeDerivation(consumer(shared, "shared-chain-" + std::to_string(level)));
    auto firstLeaf = store->writeDerivation(consumer(shared, "sibling-0"));
    auto parent = consumer(firstLeaf, "wide-parent");
    for (size_t branch = 1; branch < width; ++branch) {
        auto leaf = store->writeDerivation(consumer(shared, "sibling-" + std::to_string(branch)));
        parent.inputDrvs.map[leaf].value.insert("out");
    }
    CountingLocalStore reader{store->config};
    auto oldResolved = parent.tryResolve(reader);
    ASSERT_TRUE(oldResolved);
    auto oldOutput = oldResolved->outputs.at("out").path(reader, parent.name, "out");
    ASSERT_TRUE(oldOutput);
    // Each sibling used to start a separate memo, rereading the whole shared
    // chain. Count real derivation reads, not elapsed time or memo operations.
    constexpr size_t readBound = 4 * (depth + width + 1);
    EXPECT_LE(reader.derivationReads, readBound);

    // Fail on the last direct input, after earlier siblings populated the
    // owner's memo. A non-Nix exception escapes tryResolve's missing-input
    // handler and must still destroy the outer synchronous scope.
    reader.throwOnRead = parent.inputDrvs.map.rbegin()->first;
    EXPECT_THROW(parent.tryResolve(reader), std::runtime_error);
    reader.throwOnRead.reset();
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    reader.derivationReads = 0;
    auto newResolved = parent.tryResolve(reader);
    ASSERT_TRUE(newResolved);
    auto newOutput = newResolved->outputs.at("out").path(reader, parent.name, "out");
    ASSERT_TRUE(newOutput);
    EXPECT_NE(oldOutput, newOutput);
    EXPECT_LE(reader.derivationReads, readBound);
}

TEST_F(DeferredAliasTest, CallbackResolutionCanChangeProofBetweenLookups)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    auto parent = consumer(rootPath, "callback-parent");
    auto resolved = parent.tryResolve(
        *store, [&](ref<const SingleDerivedPath> path, const std::string & output) -> std::optional<StorePath> {
            auto request = SingleDerivedPath::Built{path, output};
            EXPECT_EQ(resolveDerivedPath(*store, request), invalid);
            store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
            auto current = resolveDerivedPath(*store, request);
            EXPECT_EQ(current, valid);
            return current;
        });
    ASSERT_TRUE(resolved);
    EXPECT_TRUE(resolved->inputSrcs.contains(valid));
    EXPECT_FALSE(resolved->inputSrcs.contains(invalid));
}

TEST_F(DeferredAliasTest, RepairsDirectAndTransitiveAliasesAfterProducerRepair)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto invalid = record("payload", true);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId(rootDrv)});
    auto direct = consumer(rootPath, "direct");
    auto directDrvPath = store->writeDerivation(direct);
    auto oldDirect = historicalConsumer(direct, invalid);
    auto transitive = consumer(directDrvPath, "transitive");
    auto transitiveDrvPath = store->writeDerivation(transitive);
    auto oldTransitive = historicalConsumer(transitive, oldDirect);

    // Use the production CA repair entrypoint, retaining the original object.
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    // The alias row has not changed, but the dependency has. An earlier
    // successful build result must not authorize publishing that old output.
    EXPECT_THROW(
        store->registerResolvedDrvOutput(
            Realisation{UnkeyedRealisation{.outPath = oldDirect}, outputId(direct)}, direct, *store),
        Error);
    EXPECT_EQ(store->queryRealisation(outputId(direct))->outPath, oldDirect);
    auto currentDirect = store->queryPartialDerivationOutputMap(directDrvPath).at("out");
    auto currentTransitive = store->queryPartialDerivationOutputMap(transitiveDrvPath).at("out");
    ASSERT_TRUE(currentDirect);
    ASSERT_TRUE(currentTransitive);
    EXPECT_NE(*currentDirect, oldDirect);
    EXPECT_NE(*currentTransitive, oldTransitive);
    build(transitiveDrvPath);
    EXPECT_EQ(store->queryRealisation(outputId(direct))->outPath, *currentDirect);
    EXPECT_EQ(store->queryRealisation(outputId(transitive))->outPath, *currentTransitive);
    EXPECT_EQ(readFile(store->toRealPath(*currentDirect)), store->printStorePath(valid));
    EXPECT_EQ(readFile(store->toRealPath(*currentTransitive)), store->printStorePath(*currentDirect));
    EXPECT_EQ(readFile(store->toRealPath(oldDirect)), store->printStorePath(invalid));
    EXPECT_EQ(readFile(store->toRealPath(oldTransitive)), store->printStorePath(oldDirect));
    auto before = lstat(store->toRealPath(*currentTransitive));
    build(transitiveDrvPath);
    EXPECT_EQ(lstat(store->toRealPath(*currentTransitive)).st_ino, before.st_ino);
}

TEST_F(DeferredAliasTest, RejectsWrongOriginalIdentityAndWrongResolvedOutput)
{
    auto rootDrv = producer();
    auto rootPath = store->writeDerivation(rootDrv);
    auto valid = record("payload", false);
    store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = valid}, outputId(rootDrv)});
    auto direct = consumer(rootPath, "direct");
    auto directDrvPath = store->writeDerivation(direct);
    auto existing = historicalConsumer(direct, valid);
    auto other = record("unrelated valid output", false);
    auto id = outputId(direct);
    EXPECT_THROW(
        store->registerResolvedDrvOutput(Realisation{UnkeyedRealisation{.outPath = other}, id}, direct, *store), Error);
    EXPECT_THROW(
        store->registerResolvedDrvOutput(
            Realisation{UnkeyedRealisation{.outPath = existing}, identity()}, direct, *store),
        Error);
    EXPECT_EQ(store->queryRealisation(id)->outPath, existing);
    auto before = lstat(store->toRealPath(existing));
    build(directDrvPath);
    EXPECT_EQ(lstat(store->toRealPath(existing)).st_ino, before.st_ino);
}

TEST_F(RetainedContentAddressTest, NormalBuildRepairsWithoutDeletingHistoricalObject)
{
    proveBuildRepair(bmNormal);
}

TEST_F(RetainedContentAddressTest, RepairBuildRetainsHistoricalObject)
{
    proveBuildRepair(bmRepair);
}

TEST_F(RetainedContentAddressTest, NormalBuildReplacesCorruptedArchive)
{
    proveBuildRepair(bmNormal, false);
}

TEST_F(RetainedContentAddressTest, RepairBuildReplacesCorruptedArchive)
{
    proveBuildRepair(bmRepair, false);
}

TEST_F(RetainedContentAddressTest, WorkerRejectsIntactNarWithInvalidAddress)
{
    auto invalid = record("same payload", true);
    auto valid = record("same payload", false);
    Worker worker{*store, *store};
    EXPECT_EQ(worker.checkPathContents(invalid), PathContentStatus::InvalidContentAddress);
    EXPECT_EQ(worker.checkPathContents(valid), PathContentStatus::Valid);
}

TEST_F(RetainedContentAddressTest, RepairsOnlyInvalidMappingAndRetainsOriginalObject)
{
    auto invalid = record("same payload", true);
    auto valid = record("same payload", false);
    registerOutput(invalid);
    EXPECT_NO_THROW(registerOutput(valid));
    EXPECT_EQ(store->queryRealisation(identity())->outPath, valid);
    EXPECT_TRUE(store->isValidPath(invalid));
    EXPECT_EQ(readFile(store->toRealPath(invalid)), "same payload");
}

TEST_F(RetainedContentAddressTest, ValidDifferentOutputStillRefuses)
{
    auto first = record("first valid payload", false);
    auto second = record("different valid payload", false);
    registerOutput(first);
    EXPECT_THROW(registerOutput(second), Error);
    EXPECT_EQ(store->queryRealisation(identity())->outPath, first);
}

TEST_F(RetainedContentAddressTest, InvalidReplacementStillRefuses)
{
    auto first = record("first valid payload", false);
    auto invalid = record("incorrect replacement", true);
    registerOutput(first);
    EXPECT_THROW(registerOutput(invalid), Error);
    EXPECT_EQ(store->queryRealisation(identity())->outPath, first);
}

TEST_F(RetainedContentAddressTest, AvailabilityIgnoresWarmMetadataAndRecoversWithoutInvalidatingIt)
{
    auto path = record("available bytes", false);
    Store & interface = *store;
    EXPECT_TRUE(interface.isValidPath(path));
    auto info = interface.queryPathInfo(path);
    auto realPath = store->toRealPath(path);
    std::filesystem::remove(realPath);
    EXPECT_FALSE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->narHash, info->narHash);
    writeFile(realPath, "available bytes");
    EXPECT_TRUE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->narHash, info->narHash);
}

TEST_F(RetainedContentAddressTest, AvailabilityRequiresRegistrationButPreservesDanglingSymlink)
{
    auto path = record("absent-target", false, true);
    Store & interface = *store;
    EXPECT_TRUE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->path, path);
    EXPECT_TRUE(interface.isValidPath(path));
    auto unregistered = store->makeFixedOutputPath(
        "unregistered",
        {.method = FileIngestionMethod::NixArchive,
         .hash = hashString(HashAlgorithm::SHA256, "unregistered"),
         .references = {}});
    std::filesystem::create_directory(store->toRealPath(unregistered));
    EXPECT_FALSE(interface.isValidPath(unregistered));
}

TEST_F(RetainedContentAddressTest, ImportRestoresRegisteredMissingEntry)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    std::filesystem::remove(store->toRealPath(path));
    ASSERT_FALSE(store->isValidPath(path));
    ASSERT_EQ(store->queryPathInfo(path)->path, path);
    StringSource source{nar.s};
    store->addToStore(info, source, NoRepair, NoCheckSigs);
    EXPECT_EQ(readFile(store->toRealPath(path)), "same payload");
    Worker worker{*store, *store};
    EXPECT_EQ(worker.checkPathContents(path), PathContentStatus::Valid);
}

TEST_F(RetainedContentAddressTest, MissingEntryRejectsBadNarAndRemovesRejectedBytes)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    std::filesystem::remove(store->toRealPath(path));
    auto offset = nar.s.find("same payload");
    ASSERT_NE(offset, std::string::npos);
    nar.s[offset] = 'X';
    StringSource source{nar.s};
    EXPECT_THROW(store->addToStore(info, source, NoRepair, NoCheckSigs), Error);
    EXPECT_FALSE(pathExists(store->toRealPath(path)));
    EXPECT_FALSE(store->isValidPath(path));
    EXPECT_EQ(store->queryPathInfo(path)->path, path);
}

TEST_F(RetainedContentAddressTest, MissingEntryRejectsBadContentAddressAndRemovesRejectedBytes)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    info.ca->hash = hashString(HashAlgorithm::SHA256, "incorrect content address");
    std::filesystem::remove(store->toRealPath(path));
    StringSource source{nar.s};
    EXPECT_THROW(store->addToStore(info, source, NoRepair, NoCheckSigs), Error);
    EXPECT_FALSE(pathExists(store->toRealPath(path)));
    EXPECT_FALSE(store->isValidPath(path));
    EXPECT_EQ(store->queryPathInfo(path)->path, path);
}

TEST_F(RetainedContentAddressTest, ImportKeepsRegisteredDanglingSymlink)
{
    auto path = record("missing-target", false, true);
    auto before = lstat(store->toRealPath(path));
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    StringSource source{nar.s};
    store->addToStore(info, source, NoRepair, NoCheckSigs);
    EXPECT_EQ(lstat(store->toRealPath(path)).st_ino, before.st_ino);
    EXPECT_EQ(std::filesystem::read_symlink(store->toRealPath(path)), "missing-target");
}

} // namespace nix
#endif
