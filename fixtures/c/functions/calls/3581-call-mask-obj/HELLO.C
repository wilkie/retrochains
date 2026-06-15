int get(void);

int lo(void) {
  return get() & 0xFF;
}
