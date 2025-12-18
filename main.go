package main

import (
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"

	"github.com/joho/godotenv"
	"github.com/redis/go-redis/v9"
)

type config struct {
	port          string
	minioEndpoint string
	minioBucket   string
	redisAddr     string
	redisPassword string
}

type server struct {
	cfg config
	rdb *redis.Client
}

func loadConfig() (config, error) {
	_ = godotenv.Load()
	cfg := config{
		port:          os.Getenv("PORT"),
		minioEndpoint: os.Getenv("MINIO_ENDPOINT"),
		minioBucket:   os.Getenv("MINIO_BUCKET"),
		redisAddr:     os.Getenv("REDIS_MASTER"),
		redisPassword: os.Getenv("REDIS_PASS"),
	}
	if cfg.port == "" {
		cfg.port = "8080"
	}
	if cfg.redisAddr != "" && !strings.Contains(cfg.redisAddr, ":") {
		log.Printf("redis address missing port, defaulting to :6379 for %s", cfg.redisAddr)
		cfg.redisAddr += ":6379"
	}
	if cfg.minioEndpoint == "" || cfg.minioBucket == "" || cfg.redisAddr == "" {
		return config{}, errors.New("missing required env vars: MINIO_ENDPOINT, MINIO_BUCKET, REDIS_MASTER")
	}
	return cfg, nil
}

func newServer(cfg config) *server {
	return &server{
		cfg: cfg,
		rdb: redis.NewClient(&redis.Options{
			Addr:     cfg.redisAddr,
			Password: cfg.redisPassword,
			DB:       0,
		}),
	}
}

func (s *server) healthHandler(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write([]byte("ok"))
}

func (s *server) StaticSiteRouter(w http.ResponseWriter, r *http.Request) {
	host := r.Host
	if host == "" {
		http.Error(w, "missing host header", http.StatusBadRequest)
		return
	}
	key := host
	fmt.Printf("looking up host %s in redis with key %s\n", host, key)
	ctx := r.Context()
	_, err := s.rdb.Get(ctx, key).Result()
	if errors.Is(err, redis.Nil) {
		http.NotFound(w, r)
		return
	}
	if err != nil {
		log.Printf("redis lookup failed for host %s: %v", host, err)
		http.Error(w, "upstream error", http.StatusInternalServerError)
		return
	}

	path := r.URL.Path
	if path == "" || path == "/" {
		path = "/index.html"
	}
	url := fmt.Sprintf("http://%s/%s/uploads/%s%s", s.cfg.minioEndpoint, s.cfg.minioBucket, host, path)
	fmt.Println(url)
	log.Printf("proxying %s %s -> %s", r.Method, path, url)

	req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
	if err != nil {
		http.Error(w, "failed to build upstream request", http.StatusInternalServerError)
		return
	}

	// Forward incoming headers to the upstream request.
	for name, values := range r.Header {
		for _, value := range values {
			req.Header.Add(name, value)
		}
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		log.Printf("upstream fetch failed: %v", err)
		http.Error(w, "upstream unavailable", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	for name, values := range resp.Header {
		for _, value := range values {
			w.Header().Add(name, value)
		}
	}
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.WriteHeader(resp.StatusCode)

	if _, err := io.Copy(w, resp.Body); err != nil {
		log.Printf("copy response body failed: %v", err)
	}
}

func main() {
	cfg, err := loadConfig()
	if err != nil {
		log.Fatalf("configuration error: %v", err)
	}
	log.Printf("starting server with port=%s minio=%s bucket=%s redis=%s", cfg.port, cfg.minioEndpoint, cfg.minioBucket, cfg.redisAddr)

	srv := newServer(cfg)
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", srv.healthHandler)
	mux.HandleFunc("/", srv.StaticSiteRouter)

	addr := ":" + cfg.port
	log.Printf("listening on %s", addr)
	if err := http.ListenAndServe(addr, mux); err != nil {
		log.Fatal(err)
	}
}
