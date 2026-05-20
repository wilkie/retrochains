int main(void) {
  int v = 5;
  int *p = &v;
  *p += 3;
  return v;
}
