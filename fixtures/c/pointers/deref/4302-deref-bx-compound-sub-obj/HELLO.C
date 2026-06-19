int g = 100;
int *gp = &g;
int main(void) {
  *gp -= 5;
  *gp -= 2000;
  return g;
}
