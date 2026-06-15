int main(void) {
  int v = 5;
  int *p = &v;
  *p += 1000;
  return v;
}
