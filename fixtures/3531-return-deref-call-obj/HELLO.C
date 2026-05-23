int get(void);

void store(int *p) {
  *p = get();
}
