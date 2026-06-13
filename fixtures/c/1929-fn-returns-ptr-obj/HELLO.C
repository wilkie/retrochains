int g = 42;
int *get_ptr(void) { return &g; }
int main(void) {
  int *p = get_ptr();
  return *p;
}
