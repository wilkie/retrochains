int g = 42;
int main(void) {
  int huge *p = &g;
  return *p;
}
