#define N 4
#define SQ(x) ((x) * (x))
#define AREA SQ(N)
int main(void) {
  int grid[N];
  int i;
  int total;
  for (i = 0; i < N; i++) {
    grid[i] = i + 1;
  }
  total = 0;
  for (i = 0; i < N; i++) {
    total = total + grid[i];
  }
  return total + AREA - N;
}
