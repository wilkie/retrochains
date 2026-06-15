int check(void);

int driver(void) {
  if (!check()) return 1;
  return 0;
}
