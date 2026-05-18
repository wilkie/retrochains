char g;
int main() {
  char *p;
  char d;
  p = &g;
  d = 3;
  *p += d;
  return *p;
}
