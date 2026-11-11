# Mockingbird's Ruby preload, written by `mockingbird proxy env` and
# loaded with `RUBYOPT=-rmockingbird_proxy` (its directory is on RUBYLIB).
# It covers the Net::HTTP clients that ignore HTTPS_PROXY and SSL_CERT_FILE
# on their own:
#   - a connection created with an explicit nil proxy
#     (`Net::HTTP.new(host, port, nil)`, as several SDKs do) uses the proxy
#     named by the environment, like one created with the default `:ENV`;
#     Ruby's own lookup decides, so NO_PROXY still applies;
#   - TLS verification trusts the local CA in MOCKINGBIRD_CA_FILE in
#     addition to whatever the caller configured (its `ca_file`, `ca_path`
#     or `cert_store`, or the default roots), also on Rubies that ignore
#     SSL_CERT_FILE (macOS's LibreSSL Ruby).
# A connection given a proxy address of its own is left alone, and
# verification is never turned off. Nothing changes when the environment
# names no proxy. Net::HTTP is patched when it is first defined, so loading
# this file requires nothing.

module MockingbirdProxy
  @ca_file = ENV['MOCKINGBIRD_CA_FILE']
  @ca = nil
  @stores = {}

  class << self
    def enabled?
      proxy = %w[HTTPS_PROXY https_proxy HTTP_PROXY http_proxy].any? do |name|
        value = ENV[name]
        value && !value.empty?
      end
      proxy && @ca_file && !@ca_file.empty?
    end

    # The local CA, read once; nil when it cannot be read.
    def ca
      return @ca unless @ca.nil?
      @ca = begin
        OpenSSL::X509::Certificate.new(File.read(@ca_file))
      rescue StandardError
        false
      end
    end

    def add_ca(store)
      store.add_cert(ca)
    rescue OpenSSL::X509::StoreError
      # Already in the store.
    end

    # A store with the caller's `ca_file`/`ca_path` (the default roots when
    # it named neither) plus the local CA.
    def store_for(ca_file, ca_path)
      key = [ca_file, ca_path]
      @stores[key] ||= begin
        store = OpenSSL::X509::Store.new
        if ca_file || ca_path
          begin
            store.add_file(ca_file) if ca_file
            store.add_path(ca_path) if ca_path
          rescue OpenSSL::X509::StoreError
            # Unreadable: verification fails as it would have.
          end
        else
          store.set_default_paths
        end
        add_ca(store)
        store
      end
    end

    def install(http_class)
      return if http_class.ancestors.include?(Connect)
      http_class.prepend(Connect)
    end
  end

  # Prepended to Net::HTTP.
  module Connect
    private

    def connect
      # An explicit nil proxy: let the environment decide, as `:ENV` does.
      self.proxy_from_env = true if !proxy_from_env? && !proxy_address
      return super unless use_ssl? && verify_mode != OpenSSL::SSL::VERIFY_NONE && MockingbirdProxy.ca

      own_store = cert_store
      if own_store
        unless own_store.instance_variable_get(:@mockingbird_ca)
          MockingbirdProxy.add_ca(own_store)
          own_store.instance_variable_set(:@mockingbird_ca, true)
        end
        return super
      end
      own_file = ca_file
      own_path = ca_path
      begin
        self.cert_store = MockingbirdProxy.store_for(own_file, own_path)
        self.ca_file = nil
        self.ca_path = nil
        super
      ensure
        self.cert_store = nil
        self.ca_file = own_file
        self.ca_path = own_path
      end
    end
  end
end

begin
  if MockingbirdProxy.enabled?
    if defined?(::Net::HTTP)
      MockingbirdProxy.install(::Net::HTTP)
    else
      # Patch Net::HTTP when its class body first opens, so the app (or
      # Bundler) still chooses which net-http to load.
      trace = TracePoint.new(:class) do |tp|
        klass = tp.self
        if klass.is_a?(Class) && klass.name == 'Net::HTTP'
          MockingbirdProxy.install(klass)
          tp.disable
        end
      end
      trace.enable
    end
  end
rescue StandardError => e
  warn "mockingbird: Ruby proxy preload skipped: #{e.message}"
end
