# Tools that act on the machine the runtime runs on. Only sessions on a server
# get them (their MCP connection carries `X-Endeavor-Host`): there, Claude's own
# file and shell tools would see the user's Mac instead of the server.

const HOST_TOOL_NAMES = Set(["list_folder", "read_file", "run_shell"])

const _LIST_FOLDER_MAX_ENTRIES = 1000
const _READ_FILE_MAX_BYTES     = 256 * 1024
const _READ_FILE_MAX_LINE      = 2000
const _SHELL_KEEP_HALF         = 15_000
const _SHELL_DRAIN_GRACE       = 2.0

function _host_path(path)
    p = path === nothing ? "" : strip(string(path))
    isempty(p) && return homedir()
    p = expanduser(p)
    return normpath(isabspath(p) ? p : joinpath(homedir(), p))
end

function _string_arg(args, name)
    value = get(args, name, nothing)
    value isa AbstractString || throw(ArgumentError("invalid_argument::$name must be a string"))
    return value
end

function _int_arg(args, name, default)
    value = get(args, name, nothing)
    value === nothing && return default
    (value isa Real && isinteger(value)) || throw(ArgumentError("invalid_argument::$name must be a whole number"))
    return Int(value)
end

_valid_text(s::AbstractString) = isvalid(s) ? String(s) : String(map(c -> isvalid(c) ? c : '�', collect(s)))

function tool_list_folder(args)
    path = _host_path(get(args, "path", nothing))
    isdir(path) || throw(ArgumentError(ispath(path) ? "not_a_folder::$path is a file, not a folder" :
                                                      "not_found::No folder at $path"))
    entries = Dict{String,Any}[]
    for name in readdir(path)
        st = try lstat(joinpath(path, name)) catch; nothing end
        st === nothing && continue
        kind = islink(st) ? "link" : isdir(st) ? "dir" : "file"
        entry = Dict{String,Any}("name" => name, "kind" => kind, "modified" => round(Int, st.mtime))
        kind == "file" && (entry["size"] = st.size)
        push!(entries, entry)
    end
    sort!(entries; by = e -> (e["kind"] != "dir", e["name"]))
    total = length(entries)
    return Dict{String,Any}(
        "path"      => path,
        "entries"   => entries[1:min(total, _LIST_FOLDER_MAX_ENTRIES)],
        "total"     => total,
        "truncated" => total > _LIST_FOLDER_MAX_ENTRIES,
    )
end

function tool_read_file(args)
    path   = _host_path(_string_arg(args, "path"))
    offset = _int_arg(args, "offset", 1)
    limit  = _int_arg(args, "limit", 2000)
    offset >= 1 || throw(ArgumentError("invalid_argument::offset is the first line to read, 1 or more"))
    limit >= 1  || throw(ArgumentError("invalid_argument::limit must be 1 or more"))
    isfile(path) || throw(ArgumentError(isdir(path) ? "not_a_file::$path is a folder; use list_folder" :
                                                      "not_found::No file at $path"))
    0x00 in open(io -> read(io, 8192), path) &&
        throw(ArgumentError("binary_file::$path is a binary file (it has NUL bytes); read_file only reads text"))

    out = IOBuffer()
    total = 0
    last_line = offset - 1
    truncated = false
    open(path) do io
        for line in eachline(io)
            total += 1
            (offset <= total < offset + limit) || continue
            if position(out) >= _READ_FILE_MAX_BYTES
                truncated = true
                continue
            end
            text = _valid_text(line)
            if length(text) > _READ_FILE_MAX_LINE
                text = first(text, _READ_FILE_MAX_LINE) * " [line cut at $_READ_FILE_MAX_LINE characters]"
                truncated = true
            end
            print(out, lpad(total, 6), '\t', text, '\n')
            last_line = total
        end
    end
    truncated |= last_line < total
    return Dict{String,Any}(
        "path"        => path,
        "text"        => String(take!(out)),
        "start_line"  => offset,
        "end_line"    => last_line,
        "total_lines" => total,
        "truncated"   => truncated,
    )
end

# Keeps the first and last _SHELL_KEEP_HALF bytes of a stream.
mutable struct _Captured
    head::Vector{UInt8}
    tail::Vector{UInt8}
    total::Int
end

_Captured() = _Captured(UInt8[], UInt8[], 0)

function _capture!(c::_Captured, bytes::AbstractVector{UInt8})
    c.total += length(bytes)
    n = min(_SHELL_KEEP_HALF - length(c.head), length(bytes))
    append!(c.head, view(bytes, 1:n))
    append!(c.tail, view(bytes, n+1:length(bytes)))
    length(c.tail) > 4 * _SHELL_KEEP_HALF && deleteat!(c.tail, 1:length(c.tail) - _SHELL_KEEP_HALF)
    return c
end

function _captured_text(c::_Captured)
    c.total <= 2 * _SHELL_KEEP_HALF && return _valid_text(String(vcat(c.head, c.tail)))
    tail = c.tail[end-_SHELL_KEEP_HALF+1:end]
    omitted = c.total - length(c.head) - length(tail)
    return _valid_text(String(c.head)) * "\n[… $omitted bytes left out …]\n" * _valid_text(String(tail))
end

function _drain(pipe)
    captured = _Captured()
    task = @async try
        while !eof(pipe)
            _capture!(captured, readavailable(pipe))
        end
    catch
    end
    return captured, task
end

function _shell_command(command::AbstractString)
    shell = get(ENV, "SHELL", "")
    if !isempty(shell) && Sys.isexecutable(shell)
        # csh and tcsh refuse -l alongside other flags; they read .cshrc without it.
        return basename(shell) in ("csh", "tcsh") ? `$shell -c $command` : `$shell -l -c $command`
    end
    return `/bin/sh -c $command`
end

function tool_run_shell(args)
    command = _string_arg(args, "command")
    isempty(strip(command)) && throw(ArgumentError("invalid_argument::command is empty"))
    cwd = _host_path(something(get(args, "cwd", nothing), session_folder(current_owner()), Some(nothing)))
    isdir(cwd) || throw(ArgumentError("not_found::No folder at $cwd"))
    timeout = clamp(_int_arg(args, "timeout_seconds", 120), 1, 600)

    out, err = Pipe(), Pipe()
    # detach=true starts the command in its own session and process group, so a
    # timeout can kill everything it started.
    cmd = Cmd(_shell_command(command); detach=true, dir=cwd)
    process = run(pipeline(cmd; stdin=devnull, stdout=out, stderr=err); wait=false)
    close(out.in)
    close(err.in)
    out_captured, out_task = _drain(out)
    err_captured, err_task = _drain(err)

    timed_out = false
    timer = Timer(timeout) do _
        if process_running(process)
            timed_out = true
            ccall(:kill, Cint, (Cint, Cint), -getpid(process), 9)
        end
    end
    wait(process)
    close(timer)
    # A background job the command started can hold the pipes open after it exits.
    timedwait(() -> istaskdone(out_task) && istaskdone(err_task), _SHELL_DRAIN_GRACE)
    close(out)
    close(err)
    wait(out_task)
    wait(err_task)

    return Dict{String,Any}(
        "exit_code" => process.termsignal == 0 ? process.exitcode : nothing,
        "stdout"    => _captured_text(out_captured),
        "stderr"    => _captured_text(err_captured),
        "timed_out" => timed_out,
        "cwd"       => cwd,
    )
end

function call_host_tool(name::AbstractString, arguments)
    name == "list_folder" && return tool_list_folder(arguments)
    name == "read_file"   && return tool_read_file(arguments)
    name == "run_shell"   && return tool_run_shell(arguments)
    throw(ArgumentError("unknown_tool::Unknown host tool: '$name'"))
end
