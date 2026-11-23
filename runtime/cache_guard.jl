# Included first by boot.jl and by the background precompile (standalone.rs), before
# anything loads or checks the runtime's packages. Plain Julia: it must run before Pkg
# or EndeavorRuntime does.
#
# A compiled file of the runtime's own packages that this user can't read makes every
# later start fail: `Base.isprecompiled` and loading open it and throw "Permission
# denied", and Julia never compiles it again by itself (EndeavorMCP #372). Julia gives
# a new cache file its source file's mode plus write, so one compiled while its source
# was gone is left `--w-------`. Removing it lets Julia compile it again.
#
# A native image with no `.ji` beside it is never used, and Julia never removes it either: its
# own eviction works from the `.ji` files (#382). It is removed too, once it is old enough
# that no precompile can still be writing it.
#
# A compiled set whose source files are gone can't be used again either: each unpacked runtime
# folder (one per build) compiles its own set, and Julia keeps up to 10 a package before it evicts
# the oldest (#382). Once the folder is removed (`remove_unused` in standalone.rs, a day after its
# last use), its set is removed. A runtime folder may be on one machine's own disk while the depot
# is shared, as on a cluster whose cache folder is per node (#528), so a source missing here may
# be there on another machine. Each start marks the sets whose source it sees (`<stem>.used`
# beside them), and a set is removed only when no start has marked it for `ENDEAVOR_UNUSED_AGE`.

# How old a native image with no `.ji` must be before it is removed: a precompile renames its
# files into place one at a time.
const ENDEAVOR_ORPHAN_AGE = 3600.0

# How long a compiled set whose source is missing here is kept after a start last saw its source.
const ENDEAVOR_UNUSED_AGE = 3 * 24 * 3600.0

# The mark beside a set whose source a start saw (its time is when).
const ENDEAVOR_USED_EXT = ".used"

const ENDEAVOR_OWN_PACKAGES = ("EndeavorRuntime", "EndeavorRuntimeWarm")
const ENDEAVOR_COMPILED_EXTS = (".ji", ".so", ".dylib", ".dll")

"""
Remove the compiled files of Endeavor's own packages in `depot` (the first entry of
DEPOT_PATH, ours, where Julia writes) that this user can't read, with the files of the
same name beside them (a `.ji` and its native image, and on macOS the image's `.dSYM`, go
together). Also remove a native image that has no `.ji` of its name and is older than
`ENDEAVOR_ORPHAN_AGE`, with its `.dSYM`, and a set whose `.ji` names a source file of the
package that no longer exists (`endeavor_source_gone`) and that no start has seen for
`ENDEAVOR_UNUSED_AGE` (a set whose source it does see is marked as seen, `<stem>.used`). Only the EndeavorRuntime and EndeavorRuntimeWarm
folders, and never in the user's own depot (`~/.julia`). Temporary files (`jl_…`) a precompile
may be writing are left alone. The paths removed.
"""
function endeavor_drop_unreadable_caches(depot::AbstractString=first(DEPOT_PATH); readable=Sys.isreadable, log::IO=stderr, now::Float64=time())
    removed, dropped = String[], String[]
    compiled = joinpath(depot, "compiled")
    (isdir(compiled) && !samefile(depot, joinpath(homedir(), ".julia"))) || return removed
    versions = try readdir(compiled; join=true) catch; return removed end
    for version in versions, pkg in ENDEAVOR_OWN_PACKAGES
        dir = joinpath(version, pkg)
        isdir(dir) || continue
        names = try readdir(dir) catch; continue end
        compiled_file(name) = !startswith(name, "jl_") && splitext(name)[2] in ENDEAVOR_COMPILED_EXTS
        bad = Set(splitext(name)[1] for name in names if compiled_file(name) && isfile(joinpath(dir, name)) && !readable(joinpath(dir, name)))
        jis = Set(splitext(name)[1] for name in names if endswith(name, ".ji"))
        old(path) = try now - mtime(path) > ENDEAVOR_ORPHAN_AGE catch; false end
        image(name) = compiled_file(name) && !endswith(name, ".ji")
        orphans = Set(splitext(name)[1] for name in names if image(name) && !(splitext(name)[1] in jis) && !(splitext(name)[1] in bad) && old(joinpath(dir, name)))
        for stem in jis
            (startswith(stem, "jl_") || stem in bad) && continue
            ji, used = joinpath(dir, stem * ".ji"), joinpath(dir, stem * ENDEAVOR_USED_EXT)
            gone = endeavor_source_gone(ji)
            if gone === false
                # Seen here: mark it, so another machine whose disk lacks this source keeps it.
                try touch(used) catch end
            elseif gone === true
                seen = try isfile(used) ? mtime(used) : mtime(ji) catch; now end
                now - seen > ENDEAVOR_UNUSED_AGE && push!(orphans, stem)
            end
        end
        # A mark whose set Julia evicted.
        union!(orphans, splitext(name)[1] for name in names if endswith(name, ENDEAVOR_USED_EXT) && !(splitext(name)[1] in jis) && old(joinpath(dir, name)))
        isempty(bad) && isempty(orphans) && continue
        for name in names
            # On macOS a native image may have a debug bundle beside it, `<stem>.dylib.dSYM`, which
            # Julia removes with it too.
            stem = compiled_file(name) || endswith(name, ENDEAVOR_USED_EXT) ? splitext(name)[1] : endswith(name, ".dylib.dSYM") ? chop(name; tail=11) : nothing
            stem === nothing && continue
            stem in bad || stem in orphans || continue
            path = joinpath(dir, name)
            try
                rm(path; force=true, recursive=true)
                # A mark is ours, not a compiled file: it goes without a word.
                endswith(name, ENDEAVOR_USED_EXT) || push!(stem in bad ? removed : dropped, path)
            catch e
                # One Julia doesn't use may still be loaded (on Windows a loaded image can't be removed): it
                # is tried again at the next start, and saying so at every start would only be noise.
                stem in bad && println(log, "Couldn't remove $path, which can't be read, so Julia may fail to start until it's removed: ", sprint(showerror, e))
            end
        end
    end
    isempty(removed) || println(log, "Removed compiled files of Endeavor's runtime that couldn't be read; Julia compiles them again: ", join(removed, ", "))
    isempty(dropped) || println(log, "Removed compiled files of Endeavor's runtime that Julia no longer uses: ", join(dropped, ", "))
    append!(removed, dropped)
    flush(log)
    return removed
end

"""
Whether the compiled file `ji` names a source file of its package (the first, the package's own
entry file) that no longer exists here by an absolute path: `true` when it's missing, `false`
when it's there, and `nothing` when the header can't be read or names a path relative to a depot
(`@depot`). Such a set is kept and not marked.
"""
function endeavor_source_gone(ji::AbstractString)
    try
        open(ji) do io
            Base.isvalid_cache_header(io) == 0 && return nothing
            includes = Base.parse_cache_header(io, ji)[2][1]
            isempty(includes) && return nothing
            file = first(includes).filename
            isabspath(file) ? !ispath(file) : nothing
        end
    catch
        nothing
    end
end
