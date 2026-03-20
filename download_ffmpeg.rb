require "net/http"
require "uri"
require "fileutils"
require "tmpdir"

URL = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-03-19-13-03/ffmpeg-N-123557-g106616f13d-win64-gpl-shared.zip"
DEST = File.join(__dir__, "ffmpeg")

def download(url, dest_io)
  uri = URI.parse(url)
  Net::HTTP.start(uri.host, uri.port, use_ssl: uri.scheme == "https") do |http|
    http.request_get(uri.request_uri) do |res|
      case res
      when Net::HTTPRedirection
        download(res["Location"], dest_io)
        return
      when Net::HTTPOK
        total = res["Content-Length"]&.to_i
        done = 0
        res.read_body do |chunk|
          dest_io.write(chunk)
          done += chunk.bytesize
          if total
            pct = done * 100 / total
            print "\r  #{done / 1024 / 1024}MB / #{total / 1024 / 1024}MB (#{pct}%)"
          end
        end
        puts
      else
        raise "HTTP #{res.code} #{res.message}"
      end
    end
  end
end

if File.directory?(DEST)
  puts "Already exists: #{DEST}"
  exit
end

puts "Downloading FFmpeg..."
Dir.mktmpdir do |tmp|
  zip_path = File.join(tmp, "ffmpeg.zip")
  File.open(zip_path, "wb") { |f| download(URL, f) }

  puts "Extracting..."
  system("powershell", "-Command",
    "Expand-Archive -Path '#{zip_path}' -DestinationPath '#{tmp}' -Force",
    exception: true)

  extracted = Dir.glob(File.join(tmp, "ffmpeg-*")).find { |f| File.directory?(f) }
  raise "Could not find extracted directory" unless extracted

  FileUtils.rm_rf(DEST)
  FileUtils.mv(extracted, DEST)
end

puts "Done: #{DEST}"
