int main(void) {
  int v = 5;
  int *p = &v;
  *p += 100;
  return v;
}
