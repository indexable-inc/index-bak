#pragma once

#include <memory>

namespace nix {

// Share dependency proofs only within a synchronous resolution. Nested map
// queries reuse the owner; returning or throwing discards all mutable mappings
// before a later build, producer repair, or independent query can observe them.
class DerivationOutputMapQueryScope
{
    struct State;
    std::unique_ptr<State> state;

public:
    DerivationOutputMapQueryScope();
    ~DerivationOutputMapQueryScope();
    DerivationOutputMapQueryScope(const DerivationOutputMapQueryScope &) = delete;
    DerivationOutputMapQueryScope & operator=(const DerivationOutputMapQueryScope &) = delete;
};

} // namespace nix
