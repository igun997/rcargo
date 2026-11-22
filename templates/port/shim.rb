#!/usr/bin/env ruby
# frozen_string_literal: true

# rcargo port shim for `{{FUNCTION}}` (Ruby 3.0+, standard library only).
#
# In your app (the only edit to the original source; delete the lines to undo). Right after the `def`:
#
#   require "<path to this shims folder>/rcargo_port"
#   def your_function(a, b) ... end
#   RcargoPort.wrap(Object, :your_function)                 # a top-level def
#   RcargoPort.wrap(MyMod.singleton_class, :x)              # a module/class method (def self.x)
#   RcargoPort.wrap(MyClass, :x)                            # an instance method
#
# Behaviour of a wrapped method (positional arguments only, no block):
#   RCARGO_PORT_RECORD=<file>  run the original and append the call to <file> (golden cases)
#   RCARGO_PORT_DISABLE=1      always run the original
#   otherwise                  ask the Rust binary; fall back to the original when the binary is
#                              missing, arguments are not plain JSON data, or the port reports `fatal`.
# Results are plain JSON types (symbols become strings). When the port reports an error, the original
# runs so it raises its own exception.
#
# Command line (FUNCTION is `name` for a top-level def, or `Const.name`):
#   ruby rcargo_port.rb oracle MODULE.rb FUNCTION CANDIDATES.jsonl CASES.jsonl
#   ruby rcargo_port.rb bench  MODULE.rb FUNCTION CASES.jsonl

require "fileutils"
require "json"

# Loaded twice (run as a script, then required by the app): the copy already loaded stays in use.
return if defined?(RcargoPort)

module RcargoPort
  BIN = ENV["RCARGO_PORT_BIN"] || File.expand_path("../target/release/{{BIN}}", __dir__)
  MAX_CASES = Integer(ENV["RCARGO_PORT_MAX_CASES"] || "500")

  @io = nil
  @broken = false
  @seen = {}

  class << self
    def plain?(v)
      case v
      when nil, true, false, String then !v.is_a?(String) || v.valid_encoding?
      when Integer then true
      when Float then v.finite?
      when Array then v.all? { |x| plain?(x) }
      when Hash then v.all? { |k, x| k.is_a?(String) && k.valid_encoding? && plain?(x) }
      else false
      end
    end

    def ask(request)
      @io = IO.popen([BIN], "r+:UTF-8") if @io.nil? || @io.closed?
      @io.puts(request)
      @io.flush
      line = @io.gets or raise IOError, "port binary exited"
      JSON.parse(line)
    end

    def accelerated(orig, args)
      return orig.call(*args) if @broken || !File.exist?(BIN) || !plain?(args)

      begin
        res = ask(JSON.generate({ "args" => args }))
      rescue SystemCallError, IOError, JSON::ParserError
        @broken = true
        return orig.call(*args)
      end
      return res["ok"] if res.key?("ok")
      # "err": fall through, so the original raises its own exception.

      orig.call(*args)
    end

    def now_ns
      Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
    end

    def case_line(key, err, result, ns)
      return nil if err.nil? && !plain?(result)

      expect = err.nil? ? { "ok" => result } : { "err" => err.message.to_s }
      %({"args":#{key},"expect":#{JSON.generate(expect)},"ns":#{ns}})
    end

    def record(orig, args, path)
      seen = (@seen[path] ||= begin
        s = {}
        File.foreach(path) { |l| s[JSON.generate(JSON.parse(l)["args"])] = true unless l.strip.empty? } if File.exist?(path)
        s
      end)
      return orig.call(*args) unless plain?(args)

      key = JSON.generate(args)
      return orig.call(*args) if seen.size >= MAX_CASES || seen.key?(key)

      t0 = now_ns
      err = nil
      result = nil
      begin
        result = orig.call(*JSON.parse(key))
      rescue StandardError => e
        err = e
      end
      line = case_line(key, err, result, now_ns - t0)
      if line
        FileUtils.mkdir_p(File.dirname(File.expand_path(path)))
        File.open(path, "a") { |f| f.puts(line) }
        seen[key] = true
      end
      raise err if err

      result
    end

    def dispatch(orig, args)
      return orig.call(*args) if ENV["RCARGO_PORT_DISABLE"] == "1"

      path = ENV["RCARGO_PORT_RECORD"]
      return record(orig, args, path) if path && !path.empty?

      accelerated(orig, args)
    end

    # Replace owner#name with a version that records or accelerates; the original stays reachable.
    def wrap(owner, name)
      orig = owner.instance_method(name)
      vis = owner.private_method_defined?(name) ? :private : :public
      owner.send(:define_method, name) do |*args, **kw, &blk|
        bound = orig.bind(self)
        next bound.call(*args, **kw, &blk) unless kw.empty? && blk.nil?

        RcargoPort.dispatch(bound, args)
      end
      owner.send(vis, name)
    end
  end

  # ---- command line -------------------------------------------------------------------------
  def self.target(function)
    if function.include?(".")
      const, meth = function.split(".", 2)
      obj = Object.const_get(const)
      abort "#{function} is not defined by the module" unless obj.respond_to?(meth)
      ->(*a) { obj.public_send(meth, *a) }
    else
      main = TOPLEVEL_BINDING.receiver
      abort "#{function} is not defined by the module" unless main.respond_to?(function, true)
      ->(*a) { main.send(function, *a) }
    end
  end

  def self.cases(file)
    File.readlines(file, chomp: true).reject { |l| l.strip.empty? }.map { |l| JSON.parse(l) }
  end

  def self.oracle(mod, function, candidates, out)
    ENV["RCARGO_PORT_DISABLE"] = "1"
    ENV.delete("RCARGO_PORT_RECORD")
    require File.expand_path(mod)
    fn = target(function)
    have = File.exist?(out) ? cases(out).map { |c| JSON.generate(c["args"]) } : []
    have = have.to_h { |k| [k, true] }
    FileUtils.mkdir_p(File.dirname(File.expand_path(out)))
    added = errors = 0
    File.open(out, "a") do |f|
      cases(candidates).each do |c|
        key = JSON.generate(c["args"])
        next if have[key]

        t0 = now_ns
        err = nil
        result = nil
        begin
          result = fn.call(*JSON.parse(key))
        rescue StandardError => e
          err = e
        end
        line = case_line(key, err, result, now_ns - t0)
        next unless line

        f.puts(line)
        have[key] = true
        added += 1
        errors += 1 if err
      end
    end
    warn "oracle: #{added} new cases (#{errors} are errors)"
  end

  def self.bench(mod, function, file)
    ENV["RCARGO_PORT_DISABLE"] = "1"
    require File.expand_path(mod)
    fn = target(function)
    abort "no port binary at #{BIN}: run `rcargo port build` first" unless File.exist?(BIN)
    calls = cases(file).map { |c| c["args"] }
    abort "no cases in #{file}" if calls.empty?
    reqs = calls.map { |a| JSON.generate({ "args" => a }) }
    best = lambda do |&run|
      3.times.map do
        t0 = now_ns
        run.call
        (now_ns - t0) / 1e9
      end.min
    end
    t_orig = best.call { calls.each { |a| begin; fn.call(*a); rescue StandardError; end } }
    t_port = best.call { reqs.each { |r| ask(r) } }
    puts format("original %.1fms, port %.1fms: %.1fx per call set (%d calls, best of 3, bridge included)",
                t_orig * 1e3, t_port * 1e3, t_orig / t_port, calls.size)
    puts "the port is not faster end to end: do not enable it (the call crosses the bridge too often, or the work is too small)" if t_port >= t_orig
  end
end

if $PROGRAM_NAME == __FILE__
  a = ARGV
  if a[0] == "oracle" && a.size == 5
    RcargoPort.oracle(*a[1..])
  elsif a[0] == "bench" && a.size == 4
    RcargoPort.bench(*a[1..])
  else
    abort "usage: rcargo_port.rb oracle MODULE.rb FUNCTION CANDIDATES.jsonl CASES.jsonl | bench MODULE.rb FUNCTION CASES.jsonl"
  end
end
