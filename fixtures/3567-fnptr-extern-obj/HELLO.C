extern int (*handler)(int);

int run(int v) {
  return handler(v);
}
