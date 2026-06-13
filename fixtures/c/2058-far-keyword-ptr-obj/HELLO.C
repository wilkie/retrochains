int g = 42;
int main(void) {
  int far *p = &g;
  return *p;
}
