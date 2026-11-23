
const SAVES = Threads.Atomic{Int}(0)

const STAGED_LEFT_AGE = 600.0

const SWEPT = Set{String}()
const SWEPT_LOCK = ReentrantLock()

function _remove_left_staged(target::String; now::Float64=time())
    prefix = "." * basename(target) * ".saving."
    dir = dirname(target)
    for name in readdir(isempty(dir) ? "." : dir)
        startswith(name, prefix) || continue
        staged = joinpath(dir, name)
        try
            isfile(staged) && now - mtime(staged) > STAGED_LEFT_AGE && rm(staged)
        catch
        end
    end
end

function Pluto.write_buffered(fn::Function, path::String)
    content = sprint(fn)
    target = islink(path) && isfile(path) ? realpath(path) : path
    if isfile(target) && stat(target).nlink > 1
        write(target, content)
        return nothing
    end
    staged = joinpath(dirname(target), "." * basename(target) * ".saving." * string(getpid()) * "." * string(Threads.atomic_add!(SAVES, 1)))
    try
        write(staged, content)
        isfile(target) && chmod(staged, filemode(target) & 0o7777)
        Base.Filesystem.rename(staged, target)
        first = lock(SWEPT_LOCK) do
            target in SWEPT ? false : (push!(SWEPT, target); true)
        end
        first && try _remove_left_staged(target) catch end
    catch
        rm(staged; force=true)
        write(target, content)
    end
    nothing
end
