int main(void) {
  int x = 0;
  int *p = &x;
  return sizeof(*p);
}
