long g;
int main(void) {
  long *p = &g;
  *p += 5;
  return 0;
}
