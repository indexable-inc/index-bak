#include <gtest/gtest.h>

#include "nix/store/sqlite.hh"
#include "nix/util/file-system.hh"
#include "nix/util/finally.hh"

#ifndef _WIN32

#  include <cerrno>
#  include <fcntl.h>
#  include <sys/wait.h>
#  include <unistd.h>

namespace nix {

// POSIX locks are process-owned: a second process must inspect them. The
// child only queries the lock table and never touches SQLite or changes locks.
static bool anotherProcessSeesLock(const std::filesystem::path & path)
{
    auto child = fork();
    if (child == -1)
        throw SysError("forking SQLite lock observer");
    if (child == 0) {
        auto fd = open(path.c_str(), O_RDWR | O_CLOEXEC);
        if (fd == -1)
            _exit(2);
        struct flock lock{};
        lock.l_type = F_WRLCK;
        lock.l_whence = SEEK_SET;
        lock.l_len = 0;
        auto result = fcntl(fd, F_GETLK, &lock);
        close(fd);
        _exit(result == -1 ? 2 : lock.l_type == F_UNLCK ? 1 : 0);
    }
    int status;
    while (waitpid(child, &status, 0) == -1)
        if (errno != EINTR)
            throw SysError("waiting for SQLite lock observer");
    if (!WIFEXITED(status) || WEXITSTATUS(status) > 1)
        throw Error("SQLite lock observer failed");
    return WEXITSTATUS(status) == 0;
}

TEST(SQLite, OpeningAnotherConnectionPreservesLiveWalLocks)
{
    auto root = createTempDir();
    Finally cleanup = [&]() { std::filesystem::remove_all(root); };
    auto database = root / "db.sqlite";
    auto sharedMemory = root / "db.sqlite-shm";
    SQLite primary{database, {.useWAL = true}};
    primary.exec("PRAGMA journal_mode = WAL; CREATE TABLE witness (value INTEGER)");
    primary.exec("BEGIN IMMEDIATE; INSERT INTO witness VALUES (1)");
    ASSERT_TRUE(anotherProcessSeesLock(sharedMemory));

    SQLite second{database, {.useWAL = true}};
    ASSERT_TRUE(anotherProcessSeesLock(sharedMemory));
    {
        SQLiteStmt read{second, "SELECT count(*) FROM witness"};
        auto rows = read.use();
        ASSERT_TRUE(rows.next());
        EXPECT_EQ(rows.getInt(0), 0);
    }
    primary.exec("COMMIT");
    {
        SQLiteStmt read{second, "SELECT count(*) FROM witness"};
        auto rows = read.use();
        ASSERT_TRUE(rows.next());
        EXPECT_EQ(rows.getInt(0), 1);
    }
}

} // namespace nix

#endif
