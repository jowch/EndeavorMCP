
const ENDEAVOR_ORPHAN_AGE = 3600.0

const ENDEAVOR_UNUSED_AGE = 3 * 24 * 3600.0

const ENDEAVOR_USED_EXT = ".used"

const ENDEAVOR_OWN_PACKAGES = ("EndeavorRuntime", "EndeavorRuntimeWarm")
const ENDEAVOR_COMPILED_EXTS = (".ji", ".so", ".dylib", ".dll")

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
                try touch(used) catch end
            elseif gone === true
                seen = try isfile(used) ? mtime(used) : mtime(ji) catch; now end
                now - seen > ENDEAVOR_UNUSED_AGE && push!(orphans, stem)
            end
        end
        for name in names
            endswith(name, ENDEAVOR_USED_EXT) && !(splitext(name)[1] in jis) && old(joinpath(dir, name)) || continue
            try rm(joinpath(dir, name); force=true) catch end
        end
        isempty(bad) && isempty(orphans) && continue
        for name in names
            stem = compiled_file(name) || endswith(name, ENDEAVOR_USED_EXT) ? splitext(name)[1] : endswith(name, ".dylib.dSYM") ? chop(name; tail=11) : nothing
            stem === nothing && continue
            stem in bad || stem in orphans || continue
            path = joinpath(dir, name)
            try
                rm(path; force=true, recursive=true)
                endswith(name, ENDEAVOR_USED_EXT) || push!(stem in bad ? removed : dropped, path)
            catch e
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
